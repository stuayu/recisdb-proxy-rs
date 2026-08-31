//! macOS: `launchctl` を介したサービス登録・制御 (`cfg(target_os = "macos")`)。
//!
//! plist の文字列生成は `super::unit_text` の純関数に委譲する。
//! `launchctl bootstrap`/`bootout` (modern) を優先し、失敗したら
//! `load -w`/`unload -w` (legacy, 古い macOS 向け) にフォールバックする。

use std::path::PathBuf;
use std::process::Command;

use super::unit_text;
use super::{ServiceError, ServiceScope, ServiceSpec, ServiceStatus};

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/root"))
}

fn plist_path(name: &str, scope: ServiceScope) -> PathBuf {
    unit_text::launchd_plist_path(scope, &home_dir(), name)
}

/// `launchctl bootstrap`/`bootout` に渡すドメインターゲット。
/// System = `system`、User = `gui/$UID` (ログインセッションのGUIドメイン)。
fn domain_target(scope: ServiceScope) -> String {
    match scope {
        ServiceScope::System => "system".to_string(),
        ServiceScope::User => {
            let uid = unsafe { libc::getuid() };
            format!("gui/{uid}")
        }
    }
}

fn io_err_to_service_err(e: std::io::Error) -> ServiceError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ServiceError::PermissionDenied
    } else {
        ServiceError::Io(e)
    }
}

fn looks_like_permission_denied(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("requires root")
}

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

fn run_ok(cmd: &mut Command) -> bool {
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn run_capture_stdout(cmd: &mut Command) -> String {
    cmd.output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    let path = plist_path(&spec.name, spec.scope);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_err_to_service_err)?;
    }
    // StandardOutPath/StandardErrorPath 用の logs ディレクトリ。作れなくても
    // launchd がサービス起動時に自動生成を試みるので、致命的エラーにはしない。
    let _ = std::fs::create_dir_all(spec.working_dir.join("logs"));

    let body = unit_text::launchd_plist_body(spec);
    std::fs::write(&path, body).map_err(io_err_to_service_err)?;

    let domain = domain_target(spec.scope);
    let path_str = path.to_string_lossy().to_string();

    // 現代の launchctl (bootstrap/bootout) を優先し、失敗時のみ legacy
    // (load/unload -w) にフォールバックする。
    if !run_ok(Command::new("launchctl").args(["bootstrap", &domain, &path_str])) {
        run_checked(
            Command::new("launchctl").args(["load", "-w", &path_str]),
            "launchctl load -w",
        )?;
    }
    // enable は「次回ログイン/起動時も自動起動する」ことを明示する
    // (bootstrap 単体では enabled にならないビルドがあるため念のため実行。
    // 失敗はベストエフォートで無視する)。
    let label = unit_text::launchd_label(&spec.name);
    let _ = Command::new("launchctl")
        .args(["enable", &format!("{domain}/{label}")])
        .output();

    Ok(())
}

pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    let domain = domain_target(scope);
    let label = unit_text::launchd_label(name);
    let path = plist_path(name, scope);
    let path_str = path.to_string_lossy().to_string();

    // bootout がダメなら legacy unload -w。どちらも「未登録なら失敗して当然」
    // なのでベストエフォート。
    if !run_ok(Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")])) {
        let _ = Command::new("launchctl")
            .args(["unload", "-w", &path_str])
            .output();
    }

    if path.exists() {
        std::fs::remove_file(&path).map_err(io_err_to_service_err)?;
    }
    Ok(())
}

pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    let domain = domain_target(scope);
    let label = unit_text::launchd_label(name);
    run_checked(
        Command::new("launchctl").args(["kickstart", "-k", &format!("{domain}/{label}")]),
        "launchctl kickstart",
    )
}

pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    let domain = domain_target(scope);
    let label = unit_text::launchd_label(name);
    run_checked(
        Command::new("launchctl").args(["kill", "SIGTERM", &format!("{domain}/{label}")]),
        "launchctl kill",
    )
}

pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    // launchctl kickstart -k は「既に動いていれば再起動」の意味を持つ。
    start(name, scope)
}

pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    let installed = plist_path(name, scope).exists();
    let domain = domain_target(scope);
    let label = unit_text::launchd_label(name);

    let output =
        run_capture_stdout(Command::new("launchctl").args(["print", &format!("{domain}/{label}")]));
    // `launchctl print` の出力に "state = running" のような行が含まれる。
    // 厳密なパーサーではなく、判断に十分な部分文字列マッチに留める。
    let running = output
        .lines()
        .any(|line| line.trim_start().starts_with("state = running"));

    ServiceStatus {
        supported: true,
        manager: "launchd".to_string(),
        name: name.to_string(),
        scope,
        installed,
        running,
        // launchctl print が成功する = ドメインに登録されている、をおおむね
        // 「enabled」相当とみなす (launchd に is-enabled 相当のサブコマンドは無い)。
        enabled: installed && !output.is_empty(),
        detail: if output.is_empty() {
            None
        } else {
            Some(output.lines().take(3).collect::<Vec<_>>().join(" | "))
        },
    }
}
