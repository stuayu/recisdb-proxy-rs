//! 「自分自身を再起動する」処理。Web API (`POST /api/service/restart`) と
//! 自己更新 (`web/api/update.rs`) の両方から使う。
//!
//! どう再起動するのが正しいかは、プロセスがサービス管理下で動いているか
//! どうかで変わる:
//!
//! - **systemd / launchd 配下**: プロセスを終了するだけでよい。
//!   `Restart=always` (systemd) / `KeepAlive=true` (launchd) が新しい
//!   プロセスを起こす。unit を操作しないので root 権限も要らない。
//! - **Windows SCM 配下**: 正常終了 (exit code 0) は SCM にとって
//!   「停止した」であって障害ではないため、failure actions による自動
//!   再起動は発火しない。そこで切り離した `cmd` から
//!   `sc stop` → `sc start` を実行させる。
//! - **サービス管理下でない (手動起動)**: 従来どおり自分自身を
//!   exec し直す (Unix) / 遅延起動用のランチャを撒いて終了する (Windows)。

use std::path::Path;

use super::{running_under_service_manager, ServiceError};

/// 再起動の実行方式 (ログ表示・APIレスポンス用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartMethod {
    /// プロセスを終了し、サービスマネージャの自動再起動に任せる。
    ServiceManagerRespawn,
    /// `sc stop` → `sc start` を切り離したプロセスから実行する (Windows)。
    ServiceControlManager,
    /// 自分自身を起動し直す (サービス管理下でないとき)。
    ExecSelf,
}

/// 現在のプロセスをどう再起動することになるかを、実際に再起動せずに返す。
pub fn restart_method() -> RestartMethod {
    if running_under_service_manager() {
        #[cfg(windows)]
        {
            if super::current_service_name().is_some() {
                return RestartMethod::ServiceControlManager;
            }
        }
        #[cfg(not(windows))]
        {
            return RestartMethod::ServiceManagerRespawn;
        }
    }
    RestartMethod::ExecSelf
}

/// 自分自身を再起動する。
///
/// 成功した場合、この関数は **戻らないことがある** (Unix の `exec`、
/// あるいは `std::process::exit`)。戻り値 `Ok(())` は「再起動の手続きを
/// 開始した」ことだけを意味する。
pub fn restart_self() -> Result<(), ServiceError> {
    match restart_method() {
        RestartMethod::ServiceManagerRespawn => {
            // systemd / launchd が起こし直す。ログを確実に吐かせるための
            // 猶予は呼び出し側 (API ハンドラ) が入れている。
            std::process::exit(0);
        }
        RestartMethod::ServiceControlManager => {
            #[cfg(windows)]
            {
                let name = super::current_service_name()
                    .ok_or(ServiceError::NotSupported)?;
                super::windows_scm::spawn_detached_restart(&name)
            }
            #[cfg(not(windows))]
            {
                Err(ServiceError::NotSupported)
            }
        }
        RestartMethod::ExecSelf => {
            let exe_path = std::env::current_exe()?;
            restart_process(&exe_path).map_err(|e| ServiceError::CommandFailed {
                command: "restart".to_string(),
                exit_code: None,
                stderr: e,
            })
        }
    }
}

/// `exe_path` をプロセスの元の引数で起動し直す。
///
/// - Unix: その場で `exec()` する。成功時はプロセスイメージが置き換わる
///   ので戻らない (戻るのは `exec` 自体が失敗したときだけ)。listen
///   ソケットは tokio が `CLOEXEC` で開いているため `exec` で閉じられ、
///   新しいイメージが再 bind できる。systemd 配下では PID/cgroup が
///   変わらないのでユニットは "active" のままになる。
/// - Windows: 新プロセスが listen ポートを旧プロセスと奪い合わないよう、
///   数秒待ってから `start` する切り離し `cmd` ランチャを撒き、この
///   プロセスは即座に終了する。
pub fn restart_process(exe_path: &Path) -> Result<(), String> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe_path).args(&args).exec();
        Err(format!("exec failed: {err}"))
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        // `ping -n 4` ≈ a 3-second delay without needing `timeout.exe`
        // (which refuses to run without a console). `start` gives the
        // restarted server its own console, matching a manual launch.
        let mut relaunch = format!(
            "/C ping -n 4 127.0.0.1 >nul & start \"recisdb-proxy\" \"{}\"",
            exe_path.display()
        );
        for arg in &args {
            relaunch.push_str(&format!(" \"{}\"", arg.to_string_lossy()));
        }

        // `raw_arg` hands the line to cmd.exe verbatim — std's per-argument
        // quoting would corrupt the `&`/`>nul` cmd metacharacters.
        match std::process::Command::new("cmd")
            .raw_arg(relaunch)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
        {
            Ok(_) => std::process::exit(0),
            Err(e) => Err(format!("failed to spawn relauncher: {e}")),
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = exe_path;
        Err("restart is not supported on this platform".to_string())
    }
}
