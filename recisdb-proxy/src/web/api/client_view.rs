//! Client-view endpoints (クライアント設定ガイド)
//!
//! The channels tab shows *physical* bon_space/bon_channel values, which are
//! not what a BonDriver client specifies: clients enumerate virtual tuning
//! spaces and channel indices from the session (EnumTuningSpace /
//! EnumChannelName, server/session.rs). These endpoints expose exactly that
//! client-facing view — built by the same `server::client_view` functions the
//! session uses — so the dashboard can show users what to put in
//! BonDriver_NetworkProxy.ini and which space/channel a client will see.
//!
//! NOTE (docs/SYSTEM_REVIEW_2026-07.md Phase 8 M3 exception): unlike the
//! rest of `web/api`, the error responses here are intentionally left as
//! plain 200 `{"success": false, ...}` (or, for the file endpoints, a raw
//! 404 `Response`) instead of being converted to `ApiError`. `web/mod.rs`'s
//! `#[cfg(test)] client_view_reports_what_a_client_will_enumerate` test
//! asserts this exact behavior (200+success:false for an unknown tuner name,
//! 404 for an unknown file `kind`) and that test file may not be edited as
//! part of this split.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;

/// List the values a client can put in `Tuner=`: tuner groups first
/// (recommended — the server picks the best free driver), then individual
/// drivers.
pub async fn get_client_view_targets(
    State(web_state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let db = web_state.database.lock().await;

    let drivers = match db.get_all_bon_drivers() {
        Ok(v) => v,
        Err(e) => return Json(json!({"success": false, "error": e.to_string()})),
    };
    let rows = match db.get_all_channels_with_drivers() {
        Ok(v) => v,
        Err(e) => return Json(json!({"success": false, "error": e.to_string()})),
    };
    drop(db);

    use std::collections::{HashMap, HashSet};

    // Distinct enabled logical channels (NID, TSID) per driver path — the
    // same identity the client view dedupes by, so this count always equals
    // the number of channels STEP 3 will show (0 ⇒ the client would see
    // nothing; the UI warns about those targets).
    let mut logical_by_path: HashMap<&str, HashSet<(i32, i32)>> = HashMap::new();
    for (ch, bd_opt) in &rows {
        if let Some(bd) = bd_opt {
            if ch.is_enabled {
                logical_by_path
                    .entry(bd.dll_path.as_str())
                    .or_default()
                    .insert((ch.nid, ch.tsid));
            }
        }
    }

    // Group members in one pass, preserving first-seen order.
    let mut group_order: Vec<&str> = Vec::new();
    let mut group_members: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in &drivers {
        if let Some(group) = d.group_name.as_deref().filter(|g| !g.trim().is_empty()) {
            let members = group_members.entry(group).or_default();
            if members.is_empty() {
                group_order.push(group);
            }
            members.push(d.dll_path.as_str());
        }
    }

    let mut targets: Vec<serde_json::Value> = Vec::new();

    for group in group_order {
        let member_paths = &group_members[group];
        let enabled_channels = member_paths
            .iter()
            .flat_map(|p| logical_by_path.get(p).into_iter().flatten())
            .collect::<HashSet<_>>()
            .len();

        targets.push(json!({
            "type": "group",
            "name": group,
            "drivers": member_paths,
            "enabled_channels": enabled_channels,
        }));
    }

    for d in &drivers {
        targets.push(json!({
            "type": "driver",
            "name": d.dll_path,
            "display_name": d.driver_name,
            "group": d.group_name,
            "enabled_channels": logical_by_path
                .get(d.dll_path.as_str())
                .map_or(0, |s| s.len()),
        }));
    }

    Json(json!({
        "success": true,
        "targets": targets,
        // For the ready-made INI sample. Host is filled in client-side from
        // location.hostname; only the port is known server-side.
        "proxy_port": web_state.proxy_listen_addr.map(|a| a.port()),
    }))
}

/// Query parameters for the client view.
#[derive(Debug, Deserialize)]
pub struct ClientViewQuery {
    /// The `Tuner=` value to preview: a DLL path, a group name, or a driver
    /// display name — resolved with the same precedence as OpenTuner in
    /// server/session.rs (path → group → display name).
    pub tuner: String,
}

/// Resolve `tuner` with the same shared resolver OpenTuner uses
/// (`Database::resolve_tuner_target`) and load the channel rows. Unlike the
/// session, an unknown name is an error here (the guide must not silently
/// show some other driver's channels the way the session's first-driver
/// fallback does). Shared by `get_client_view` and `get_client_view_file`.
async fn resolve_client_view_scope(
    web_state: &WebState,
    tuner: &str,
) -> Result<
    (
        Vec<String>,
        &'static str,
        Vec<(crate::database::ClientChannelRecord, Option<crate::database::BonDriverRecord>)>,
    ),
    Json<serde_json::Value>,
> {
    let db = web_state.database.lock().await;

    let (driver_paths, resolved_type) = match db.resolve_tuner_target(tuner) {
        Ok(Some((paths, true))) => (paths, "group"),
        Ok(Some((paths, false))) => (paths, "driver"),
        Ok(None) => {
            return Err(Json(json!({
                "success": false,
                "error": format!("Tuner '{}' はDLLパス/グループ名/表示名のいずれにも一致しません", tuner),
            })));
        }
        Err(e) => return Err(Json(json!({"success": false, "error": e.to_string()}))),
    };

    let rows = db
        .get_all_channels_with_drivers()
        .map_err(|e| Json(json!({"success": false, "error": e.to_string()})))?;

    Ok((driver_paths, resolved_type, rows))
}

/// Return the tuning spaces and channels exactly as a client that opens
/// `tuner` will enumerate them, with the space/channel *indices* the client
/// passes to SetChannel.
pub async fn get_client_view(
    State(web_state): State<Arc<WebState>>,
    Query(query): Query<ClientViewQuery>,
) -> impl IntoResponse {
    use crate::server::client_view;

    let (driver_paths, resolved_type, rows) =
        match resolve_client_view_scope(&web_state, &query.tuner).await {
            Ok(v) => v,
            Err(e) => return e,
        };

    let driver_matches = |path: &str| driver_paths.iter().any(|p| p == path);
    let space_result = client_view::build_space_list(&rows, driver_matches);
    // One pass over the rows for every space's channel list.
    let mut channels_by_region = client_view::build_channels_by_region(&rows, driver_matches);

    let spaces: Vec<serde_json::Value> = space_result
        .spaces
        .iter()
        .enumerate()
        .map(|(space_index, space)| {
            let channels: Vec<serde_json::Value> =
                channels_by_region
                    .remove(space.region_key.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .map(|(channel_index, ch)| {
                        let physical: Vec<serde_json::Value> = space_result
                            .nid_tsid_mappings
                            .get(&(ch.nid, ch.tsid))
                            .map(|mappings| {
                                mappings
                                    .iter()
                                    .map(|m| {
                                        json!({
                                            "driver": m.driver_path,
                                            "space": m.actual_space,
                                            "channel": m.actual_channel,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        json!({
                            "index": channel_index,
                            "name": ch.name,
                            "nid": ch.nid,
                            "tsid": ch.tsid,
                            "bon_channel": ch.bon_channel,
                            "physical": physical,
                        })
                    })
                    .collect();
            json!({
                "index": space_index,
                "name": space.display_name,
                "region_key": space.region_key,
                "channels": channels,
            })
        })
        .collect();

    Json(json!({
        "success": true,
        "tuner": query.tuner,
        "resolved_type": resolved_type,
        "driver_paths": driver_paths,
        "spaces": spaces,
    }))
}

/// Download a client channel-configuration file generated from the same
/// enumeration the client will see (web/channel_files.rs):
/// `kind` = `tvtest-ch2` | `chset4` | `chset5` | `bundle` (zip with all of
/// them plus a ready-made BonDriver_NetworkProxy.ini and README).
pub async fn get_client_view_file(
    State(web_state): State<Arc<WebState>>,
    Path(kind): Path<String>,
    Query(query): Query<ClientViewQuery>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use crate::web::channel_files as cf;
    use axum::http::header;

    let (driver_paths, _resolved_type, rows) =
        match resolve_client_view_scope(&web_state, &query.tuner).await {
            Ok(v) => v,
            Err(e) => return (StatusCode::NOT_FOUND, e).into_response(),
        };

    let spaces = cf::assemble_spaces(&rows, |p| driver_paths.iter().any(|q| q == p));
    if spaces.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "error": "このチューナーには有効なチャンネルがありません。先にチャンネルスキャンを実行してください",
            })),
        )
            .into_response();
    }

    let (filename, content_type, bytes): (&str, &str, Vec<u8>) = match kind.as_str() {
        "tvtest-ch2" => (
            cf::TVTEST_CH2_FILENAME,
            "application/octet-stream",
            cf::encode_ch2(&cf::generate_tvtest_ch2(&spaces)),
        ),
        "chset4" => (
            cf::CHSET4_FILENAME,
            "text/plain; charset=utf-8",
            cf::encode_utf8_bom(&cf::generate_chset4(&spaces)),
        ),
        "chset5" => (
            cf::CHSET5_FILENAME,
            "text/plain; charset=utf-8",
            cf::encode_utf8_bom(&cf::generate_chset5(&spaces)),
        ),
        "bundle" => {
            match build_client_bundle_zip(&web_state, &headers, &query.tuner, &spaces) {
                Ok(bytes) => ("recisdb-proxy-client-config.zip", "application/zip", bytes),
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"success": false, "error": e})),
                    )
                        .into_response();
                }
            }
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"success": false, "error": format!("unknown file kind '{kind}'")})),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Build the "まとめてダウンロード" zip: channel files + a ready-made
/// BonDriver_NetworkProxy.ini (Address derived from the request's Host
/// header + the proxy's BNDP port, Tuner = the selected target) + README.
fn build_client_bundle_zip(
    web_state: &WebState,
    headers: &axum::http::HeaderMap,
    tuner: &str,
    spaces: &[crate::web::channel_files::FileSpace],
) -> Result<Vec<u8>, String> {
    use crate::web::channel_files as cf;
    use std::io::Write;

    // Host the browser reached the dashboard on — the best guess for the
    // address clients can reach the proxy on. Strip any port; the BNDP
    // port comes from the proxy listener, not the web listener.
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| {
            if let Some(end) = h.strip_prefix('[').and_then(|_| h.find(']')) {
                // Bracketed IPv6 literal ([::1] or [::1]:40080) — keep brackets.
                h[..=end].to_string()
            } else {
                h.rsplit_once(':').map_or(h, |(host, _)| host).to_string()
            }
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let proxy_port = web_state
        .proxy_listen_addr
        .map(|a| a.port())
        .unwrap_or(40070);
    let server_addr = format!("{host}:{proxy_port}");
    let dashboard_url = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| format!("http://{h}"))
        .unwrap_or_else(|| "http://127.0.0.1:40080".to_string());

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default();
        let mut add = |name: &str, data: &[u8]| -> Result<(), String> {
            zip.start_file(name, opts).map_err(|e| e.to_string())?;
            zip.write_all(data).map_err(|e| e.to_string())
        };

        add(
            "BonDriver_NetworkProxy.ini",
            crate::setup_helpers::generate_client_ini(&server_addr, tuner).as_bytes(),
        )?;
        add(
            cf::TVTEST_CH2_FILENAME,
            &cf::encode_ch2(&cf::generate_tvtest_ch2(spaces)),
        )?;
        add(
            cf::CHSET4_FILENAME,
            &cf::encode_utf8_bom(&cf::generate_chset4(spaces)),
        )?;
        add(
            cf::CHSET5_FILENAME,
            &cf::encode_utf8_bom(&cf::generate_chset5(spaces)),
        )?;
        add(
            "README.txt",
            crate::setup_helpers::generate_client_readme(&server_addr, &dashboard_url, true)
                .as_bytes(),
        )?;
        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}
