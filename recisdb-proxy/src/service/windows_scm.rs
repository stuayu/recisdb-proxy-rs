//! Windows: SCM (Service Control Manager) を介したサービス登録・制御
//! (`cfg(windows)`)。`windows-service` クレートを使う。
//!
//! Windows の SCM にはユーザースコープの概念がない (`ServiceScope::User`
//! は常に `NotSupported`)。
//!
//! **注意**: このファイルは非Windows環境 (macOSでの開発機) では
//! コンパイルされないため、CIないし実機Windowsビルドでのコンパイル確認が
//! 別途必要。`windows-service = "0.7"` の公開APIに基づいて書いているが、
//! 型名/メソッド名の細部はクレートのドキュメントで最終確認すること
//! (実装者注: このセッションではWindowsターゲットでのビルド検証ができて
//! いない)。

use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::{ServiceError, ServiceScope, ServiceSpec, ServiceStatus, SERVICE_WORKDIR_FLAG};


fn to_windows_service_error(e: windows_service::Error) -> ServiceError {
    // アクセス拒否 (ERROR_ACCESS_DENIED = 5) は「管理者として実行してください」
    // という案内に倒す。windows-service は std::io::Error をラップして返す
    // ケースが多いので、メッセージにも "Access is denied" が出ることがある。
    let msg = e.to_string();
    if msg.contains("Access is denied") || msg.contains("os error 5") {
        ServiceError::PermissionDenied
    } else {
        ServiceError::CommandFailed {
            command: "windows-service".to_string(),
            exit_code: None,
            stderr: msg,
        }
    }
}

fn require_system_scope(scope: ServiceScope) -> Result<(), ServiceError> {
    match scope {
        ServiceScope::System => Ok(()),
        ServiceScope::User => Err(ServiceError::NotSupported),
    }
}

/// SCM の制御 API は要求を受け付けた時点で戻るため、呼び出し側が
/// `STOP_PENDING` のまま `start` しないよう、状態が確定するまで待つ。
/// 特に BonDriver の解放には数秒かかることがあり、ここを待たないと
/// `service restart` が「開始済み」に見えても実プロセスが残ったままになる。
fn wait_for_state(
    service: &windows_service::service::Service,
    expected: ServiceState,
) -> Result<(), ServiceError> {
    const TIMEOUT: Duration = Duration::from_secs(60);
    let deadline = std::time::Instant::now() + TIMEOUT;

    loop {
        let status = service.query_status().map_err(to_windows_service_error)?;
        if status.current_state == expected {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ServiceError::CommandFailed {
                command: "windows-service wait".to_string(),
                exit_code: None,
                stderr: format!(
                    "timed out waiting for service state {:?} (current: {:?})",
                    expected, status.current_state
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `spec.args` の先頭に `--run-as-service --service-workdir <dir>` を付与した
/// launch_arguments を組み立てる。
fn launch_arguments(spec: &ServiceSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from(SERVICE_WORKDIR_FLAG),
        spec.working_dir.clone().into_os_string(),
    ];
    // `--run-as-service --service-name <name>` + 利用者指定の引数。
    args.extend(spec.service_args().iter().map(OsString::from));
    args
}

pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    require_system_scope(spec.scope)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(to_windows_service_error)?;

    let service_info = ServiceInfo {
        name: OsString::from(&spec.name),
        display_name: OsString::from(&spec.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: spec.exe_path.clone(),
        launch_arguments: launch_arguments(spec),
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let service = manager
        .create_service(
            &service_info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )
        .map_err(to_windows_service_error)?;

    let _ = service.set_description(&spec.description);

    // 5秒後に再起動、3回まで、1日でリセット。SCM 障害時に落ちっぱなしに
    // ならないようにする (systemd の Restart=always/RestartSec=5 相当)。
    let failure_actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
        ]),
    };
    let _ = service.update_failure_actions(failure_actions);

    service.start(&Vec::<&OsStr>::new()).map_err(to_windows_service_error)?;
    Ok(())
}

pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    require_system_scope(scope)?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_windows_service_error)?;
    let service = manager
        .open_service(
            name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(to_windows_service_error)?;

    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
        }
    }
    service.delete().map_err(to_windows_service_error)
}

pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    require_system_scope(scope)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_windows_service_error)?;
    let service = manager
        .open_service(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(to_windows_service_error)?;
    service.start(&Vec::<&OsStr>::new()).map_err(to_windows_service_error)?;
    wait_for_state(&service, ServiceState::Running)
}

pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    require_system_scope(scope)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_windows_service_error)?;
    let service = manager
        .open_service(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .map_err(to_windows_service_error)?;
    if service.query_status().map_err(to_windows_service_error)?.current_state
        == ServiceState::Stopped
    {
        return Ok(());
    }
    service.stop().map_err(to_windows_service_error)?;
    wait_for_state(&service, ServiceState::Stopped)
}

pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    stop(name, scope)?;
    start(name, scope)
}

pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    if scope == ServiceScope::User {
        return ServiceStatus {
            supported: false,
            manager: "windows-scm".to_string(),
            name: name.to_string(),
            scope,
            installed: false,
            running: false,
            enabled: false,
            detail: Some("Windows SCM does not support a user scope".to_string()),
        };
    }

    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(m) => m,
        Err(e) => {
            return ServiceStatus {
                supported: true,
                manager: "windows-scm".to_string(),
                name: name.to_string(),
                scope,
                installed: false,
                running: false,
                enabled: false,
                detail: Some(format!("failed to connect to SCM: {e}")),
            };
        }
    };

    let service = match manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG) {
        Ok(s) => s,
        Err(_) => {
            // 未登録 (または権限不足)。installed=false で返す。
            return ServiceStatus {
                supported: true,
                manager: "windows-scm".to_string(),
                name: name.to_string(),
                scope,
                installed: false,
                running: false,
                enabled: false,
                detail: None,
            };
        }
    };

    let status = service.query_status().ok();
    let running = status
        .as_ref()
        .map(|s| s.current_state == ServiceState::Running)
        .unwrap_or(false);
    let config = service.query_config().ok();
    let enabled = config
        .as_ref()
        .map(|c| c.start_type == ServiceStartType::AutoStart)
        .unwrap_or(true);

    ServiceStatus {
        supported: true,
        manager: "windows-scm".to_string(),
        name: name.to_string(),
        scope,
        installed: true,
        running,
        enabled,
        detail: status.map(|s| format!("{:?}", s.current_state)),
    }
}

/// 自分自身をサービスとして再起動する。
///
/// `sc stop` は自プロセスを止めるので、自分の中では実行できない (停止
/// 途中で `sc start` を出す主体が居なくなる)。切り離した `cmd` に
/// `sc stop` → `sc start` を順に実行させ、この関数はすぐ戻る。実際の
/// 停止は SCM から Stop 制御として届き、ディスパッチャが graceful に
/// 落としてくれる。
///
/// `name` は SCM 登録名。呼び出し元 (`restart.rs`) は
/// `current_service_name()` 由来の値しか渡さず、その値は
/// `sanitize_service_name` を通した名前が `--service-name` で戻って
/// きたものなので、コマンドラインに埋め込んでも安全。
pub fn spawn_detached_restart(name: &str) -> Result<(), ServiceError> {
    // 防御的に再検証する: ここで組み立てる文字列は cmd.exe が解釈する。
    let name = super::sanitize_service_name(name)?;

    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // `ping -n 3` ≈ 2秒待ってから停止 (このレスポンスを返し切るため)、
    // 停止後さらに待ってから開始する。
    let line = format!(
        "/C ping -n 3 127.0.0.1 >nul & sc stop \"{name}\" >nul & ping -n 6 127.0.0.1 >nul & sc start \"{name}\" >nul"
    );

    std::process::Command::new("cmd")
        .raw_arg(line)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
        .map_err(|e| ServiceError::CommandFailed {
            command: "cmd /C sc stop && sc start".to_string(),
            exit_code: None,
            stderr: e.to_string(),
        })
}

// ---------------------------------------------------------------------
// SCM ディスパッチャ: `recisdb-proxy --run-as-service` 経路で
// ServiceMain として動く。
// ---------------------------------------------------------------------

use windows_service::service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceStatus as WinServiceStatus};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// `run_dispatcher` に渡すサーバ本体。呼ばれたら実サーバを起動し、
/// `should_stop` が真になったら (または内部で shutdown を検知したら)
/// 戻ってくることが期待される。Send + 'static。
pub trait ServiceMainBody: FnOnce(std::sync::Arc<AtomicBool>) + Send + 'static {}
impl<F: FnOnce(std::sync::Arc<AtomicBool>) + Send + 'static> ServiceMainBody for F {}

// windows-service の define_windows_service! はグローバル関数を要求する
// ため、実行するクロージャをプロセスグローバルに保持する。
static SERVICE_NAME_STORAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static MAIN_BODY_STORAGE: std::sync::OnceLock<
    std::sync::Mutex<Option<Box<dyn FnOnce(std::sync::Arc<AtomicBool>) + Send>>>,
> = std::sync::OnceLock::new();

define_windows_service!(ffi_service_main, service_main_entry);

fn service_main_entry(_arguments: Vec<OsString>) {
    let service_name = SERVICE_NAME_STORAGE.get().cloned().unwrap_or_default();
    let should_stop = std::sync::Arc::new(AtomicBool::new(false));
    let should_stop_for_handler = std::sync::Arc::clone(&should_stop);

    let status_handle = match service_control_handler::register(
        &service_name,
        move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    should_stop_for_handler.store(true, Ordering::SeqCst);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        },
    ) {
        Ok(h) => h,
        Err(_) => return,
    };

    let report = |state: ServiceState, accept: ServiceControlAccept, checkpoint: u32| {
        let _ = status_handle.set_service_status(WinServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        });
    };

    report(ServiceState::StartPending, ServiceControlAccept::empty(), 1);

    if let Some(body) = MAIN_BODY_STORAGE
        .get()
        .and_then(|m| m.lock().ok().and_then(|mut g| g.take()))
    {
        report(ServiceState::Running, ServiceControlAccept::STOP, 0);
        // サーバ本体を実行。`should_stop` を見て graceful shutdown する
        // 責務は呼び出し元 (main.rs 側の run_server) にある。この呼び出し
        // はサーバが終了するまでブロックする。
        body(should_stop);
        report(ServiceState::StopPending, ServiceControlAccept::empty(), 1);
    }

    report(ServiceState::Stopped, ServiceControlAccept::empty(), 0);
}

/// SCM からのディスパッチを開始する。`main_body` はサーバ本体を起動する
/// クロージャで、`Arc<AtomicBool>` (stop要求フラグ) を受け取り、サーバが
/// 停止するまでブロックして戻ることが期待される。
///
/// この関数は `service_dispatcher::start` が返るまで (= サービスが
/// 停止するまで) ブロックする。
pub fn run_dispatcher(
    service_name: &str,
    main_body: impl FnOnce(std::sync::Arc<AtomicBool>) + Send + 'static,
) -> windows_service::Result<()> {
    let _ = SERVICE_NAME_STORAGE.set(service_name.to_string());
    let _ = MAIN_BODY_STORAGE.set(std::sync::Mutex::new(Some(Box::new(main_body))));
    service_dispatcher::start(service_name, ffi_service_main)
}
