//! Linux: `systemctl` を介したサービス登録・制御 (`cfg(target_os = "linux")`)。
//!
//! unit ファイルの文字列生成は `super::unit_text` の純関数に委譲し、ここ
//! ではファイル I/O と `systemctl` の起動のみを行う。コマンドは常に
//! `Command::new("systemctl").arg(...)` の形式で組み立て、シェル
//! (`sh -c`) は一切経由しない。

use std::path::PathBuf;
use std::process::Command;

use super::unit_text;
use super::{ServiceError, ServiceScope, ServiceSpec, ServiceStatus};

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

fn xdg_config_home() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn unit_path(name: &str, scope: ServiceScope) -> PathBuf {
    match scope {
        ServiceScope::System => unit_text::systemd_system_unit_path(name),
        ServiceScope::User => {
            unit_text::systemd_user_unit_path(&home_dir(), xdg_config_home().as_deref(), name)
        }
    }
}

/// `systemctl` (System) または `systemctl --user` (User) の `Command` を
/// 組み立てる。呼び出し側がさらに `.arg(...)` / `.args(...)` を続ける。
fn systemctl(scope: ServiceScope) -> Command {
    let mut cmd = Command::new("systemctl");
    if scope == ServiceScope::User {
        cmd.arg("--user");
    }
    cmd
}

/// 権限不足を示す典型的なメッセージを検出する。systemctl は権限不足時に
/// polkit 経由の対話認証を要求 ("Interactive authentication required")
/// するか、直接 "Permission denied" を返すことがある。
fn looks_like_permission_denied(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("interactive authentication required")
        || lower.contains("not authorized")
}

fn io_err_to_service_err(e: std::io::Error) -> ServiceError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ServiceError::PermissionDenied
    } else {
        ServiceError::Io(e)
    }
}

/// コマンドを実行し、失敗した場合は `PermissionDenied` か `CommandFailed`
/// に変換する。`label` はエラーメッセージ用の識別子 (例: "systemctl start")。
fn run_checked(cmd: &mut Command, label: &str) -> Result<(), ServiceError> {
    let out = cmd.output().map_err(io_err_to_service_err)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if looks_like_permission_denied(&stderr) {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::CommandFailed {
        command: label.to_string(),
        exit_code: out.status.code(),
        stderr,
    })
}

/// 標準出力の1行目 (前後空白を除去) を返す。取得失敗時は空文字列。
fn run_capture_stdout(cmd: &mut Command) -> String {
    cmd.output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    let path = unit_path(&spec.name, spec.scope);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_err_to_service_err)?;
    }
    let body = unit_text::systemd_unit_body(spec);
    std::fs::write(&path, body).map_err(io_err_to_service_err)?;

    run_checked(
        systemctl(spec.scope).arg("daemon-reload"),
        "systemctl daemon-reload",
    )?;
    run_checked(
        systemctl(spec.scope).args(["enable", &spec.name]),
        "systemctl enable",
    )?;
    run_checked(
        systemctl(spec.scope).args(["start", &spec.name]),
        "systemctl start",
    )?;
    Ok(())
}

pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    // stop/disable は「そもそも登録されていない」場合も非0で返り得るので
    // ベストエフォート (エラーは無視して、ファイル削除まで進む)。
    let _ = systemctl(scope).args(["stop", name]).output();
    let _ = systemctl(scope).args(["disable", name]).output();

    let path = unit_path(name, scope);
    if path.exists() {
        std::fs::remove_file(&path).map_err(io_err_to_service_err)?;
    }
    run_checked(
        systemctl(scope).arg("daemon-reload"),
        "systemctl daemon-reload",
    )?;
    Ok(())
}

pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    run_checked(systemctl(scope).args(["start", name]), "systemctl start")
}

pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    run_checked(systemctl(scope).args(["stop", name]), "systemctl stop")
}

pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    run_checked(
        systemctl(scope).args(["restart", name]),
        "systemctl restart",
    )
}

pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    let installed = unit_path(name, scope).exists();
    let active_state = run_capture_stdout(systemctl(scope).args(["is-active", name]));
    let enabled_state = run_capture_stdout(systemctl(scope).args(["is-enabled", name]));

    ServiceStatus {
        supported: true,
        manager: "systemd".to_string(),
        name: name.to_string(),
        scope,
        installed,
        running: active_state == "active",
        enabled: enabled_state == "enabled",
        detail: Some(format!(
            "ActiveState={active_state}, UnitFileState={enabled_state}"
        )),
    }
}
