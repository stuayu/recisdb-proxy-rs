//! OSサービス連携 (`/api/service/*`)。
//!
//! - `GET  /api/service/status`  — 登録状況・稼働状況と、いま自分が
//!   サービス管理下で動いているかを返す。
//! - `POST /api/service/restart` — サーバ自身を再起動する。
//!
//! サービスの **登録/削除** はここには置いていない。管理者権限が要る操作
//! であり、Web API (= ネットワーク越し) から任意のパスを実行ファイルとして
//! 登録できてしまうのは権限昇格の経路になるため、セットアップGUIと
//! `recisdb-proxy service install` CLI からのみ行う。

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::service::{self, ServiceScope};
use crate::web::state::WebState;

use super::error::ApiError;

#[derive(Debug, Deserialize)]
pub struct ServiceQuery {
    /// 問い合わせるサービス名。省略時は自分の登録名 (サービスとして起動
    /// されている場合)、それも無ければ既定名。
    pub name: Option<String>,
    /// `user` を指定するとユーザースコープを問い合わせる。
    pub scope: Option<String>,
}

fn resolve_name(requested: Option<&str>) -> Result<String, ApiError> {
    let raw = requested
        .map(str::to_string)
        .or_else(service::current_service_name)
        .unwrap_or_else(|| service::DEFAULT_SERVICE_NAME.to_string());
    service::sanitize_service_name(&raw).map_err(|e| ApiError::bad_request(e.to_string()))
}

fn resolve_scope(requested: Option<&str>) -> ServiceScope {
    match requested {
        Some(s) if s.eq_ignore_ascii_case("user") => ServiceScope::User,
        _ => ServiceScope::System,
    }
}

/// `GET /api/service/status`
pub async fn get_service_status(
    Query(query): Query<ServiceQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = resolve_name(query.name.as_deref())?;
    let scope = resolve_scope(query.scope.as_deref());

    // `systemctl`/`launchctl` の呼び出しはブロッキングなので、非同期
    // ランタイムのワーカーを塞がないよう別スレッドに逃がす。
    let status = tokio::task::spawn_blocking(move || service::status(&name, scope))
        .await
        .map_err(|e| ApiError::internal(format!("status task panicked: {e}")))?;

    Ok(Json(json!({
        "success": true,
        "supported": service::is_supported(),
        "running_under_service_manager": service::running_under_service_manager(),
        "restart_method": service::restart_method(),
        "service": status,
    })))
}

/// `POST /api/service/restart`
///
/// 応答を返し切ってから再起動したいので、実際の再起動は少し遅らせた
/// バックグラウンドタスクで行う。`service::restart_self` は方式を自動で
/// 選ぶ (systemd/launchd 配下なら終了して起こし直してもらう、Windows
/// サービスなら `sc stop`→`sc start`、それ以外は自分を起動し直す)。
pub async fn restart_service(
    State(_web_state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let method = service::restart_method();

    tokio::spawn(async move {
        // 応答の送信とログのフラッシュを待つ。
        tokio::time::sleep(Duration::from_millis(700)).await;
        tracing::warn!("restart requested via Web API (method: {:?})", method);
        // 実際の再起動はブロッキング (exec / process::exit / spawn)。
        let result =
            tokio::task::spawn_blocking(|| service::restart_self().map_err(|e| e.to_string()))
                .await;
        match result {
            Ok(Ok(())) => tracing::info!("restart procedure started"),
            Ok(Err(e)) => tracing::error!("restart failed: {e}"),
            Err(e) => tracing::error!("restart task panicked: {e}"),
        }
    });

    Ok(Json(json!({
        "success": true,
        "message": "restart scheduled",
        "restart_method": method,
    })))
}
