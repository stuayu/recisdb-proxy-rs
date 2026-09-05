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
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
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
/// 既定の状態待ち上限。`sc start` / `sc stop` の同期待ちに使う。
const DEFAULT_STATE_TIMEOUT: Duration = Duration::from_secs(60);

fn wait_for_state(
    service: &windows_service::service::Service,
    expected: ServiceState,
    timeout: Duration,
) -> Result<(), ServiceError> {
    let deadline = std::time::Instant::now() + timeout;

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

    service
        .start(&Vec::<&OsStr>::new())
        .map_err(to_windows_service_error)?;
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
    service
        .start(&Vec::<&OsStr>::new())
        .map_err(to_windows_service_error)?;
    wait_for_state(&service, ServiceState::Running, DEFAULT_STATE_TIMEOUT)
}

pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    require_system_scope(scope)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_windows_service_error)?;
    let service = manager
        .open_service(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .map_err(to_windows_service_error)?;
    if service
        .query_status()
        .map_err(to_windows_service_error)?
        .current_state
        == ServiceState::Stopped
    {
        return Ok(());
    }
    service.stop().map_err(to_windows_service_error)?;
    wait_for_state(&service, ServiceState::Stopped, DEFAULT_STATE_TIMEOUT)
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

    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
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

    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
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
    // 防御的に再検証する: 引数はこのあとコマンドラインに載る。
    let name = super::sanitize_service_name(name)?;

    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // 自分自身を「再起動係」として切り離して起動する。
    //
    // かつては `cmd /C ping … & sc stop & ping … & sc start` を撒いていたが、
    // `sc stop` は非同期で、固定待ち (約5秒) のあと状態を確認せずに
    // `sc start` を撃っていた。停止が終わっていなければ start は
    // `ERROR_SERVICE_ALREADY_RUNNING` / `ERROR_SERVICE_REQUEST_TIMEOUT` で
    // 失敗し、サービスは停止したまま取り残される。実際に本番で
    // 「更新したのに再起動されない」「サービスが落ちたまま」を起こしていた。
    //
    // 状態をポーリングする再起動は cmd のバッチで書くと壊れやすいので、
    // 隠しフラグ付きで自分自身を起動して Rust 側で待つ。
    // 自己更新の直後に呼ばれるため、`current_exe()` は **更新後の** exe を
    // 指す。再起動係自体は新しいバイナリで動く。
    let exe = std::env::current_exe().map_err(ServiceError::Io)?;

    std::process::Command::new(exe)
        .arg(RESTART_WATCHDOG_FLAG)
        .arg(&name)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
        .map_err(|e| ServiceError::CommandFailed {
            command: format!("{RESTART_WATCHDOG_FLAG} {name}"),
            exit_code: None,
            stderr: e.to_string(),
        })
}

/// 切り離しプロセスとして「停止を待ってから起動する」モードに入る隠しフラグ。
/// 値は次の引数 (サービス名)。
pub const RESTART_WATCHDOG_FLAG: &str = "--service-restart-watchdog";

/// 停止完了を待つ上限。サービス側の `GRACEFUL_STOP_TIMEOUT` に、強制終了
/// してから SCM が状態を反映するまでの余裕を足した値。
const RESTART_STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// 起動要求を出してから RUNNING になるまで待つ上限。
const RESTART_START_TIMEOUT: Duration = Duration::from_secs(90);
/// 起動要求のリトライ回数。停止直後は SCM がまだ後片付け中で
/// `ERROR_SERVICE_MARKED_FOR_DELETE` などを返すことがある。
const RESTART_START_ATTEMPTS: u32 = 5;

/// `--service-restart-watchdog <name>` で起動されたときの本体。
///
/// 1. 対象サービスへ停止を要求する (すでに停止していれば飛ばす)
/// 2. **STOPPED になるまで待つ** — ここが固定待ちだったのが従来の不具合
/// 3. 起動を要求し、RUNNING になるまで待つ。失敗したら間を空けて再試行
///
/// 成否は終了コードで返す (0=成功)。呼び出し元はすでに終了しているので、
/// 経過は標準エラー出力へ書く。
pub fn run_restart_watchdog(name: &str) -> Result<(), ServiceError> {
    let name = super::sanitize_service_name(name)?;
    let manager = ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::ENUMERATE_SERVICE,
    )
    .map_err(to_windows_service_error)?;
    let service = manager
        .open_service(
            &name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START,
        )
        .map_err(to_windows_service_error)?;

    // 呼び出し元 (更新API) がレスポンスを返し切る余裕。
    std::thread::sleep(Duration::from_secs(2));

    let current = service.query_status().map_err(to_windows_service_error)?;
    if current.current_state != ServiceState::Stopped {
        // 停止要求は「すでに停止処理中」なら失敗しうる。その場合も
        // 待ちには入る (エラーで抜けない)。
        if let Err(e) = service.stop() {
            eprintln!("restart watchdog: stop request returned {e}; waiting for STOPPED anyway");
        }
    }
    wait_for_state(&service, ServiceState::Stopped, RESTART_STOP_TIMEOUT)?;

    let mut last_error = String::new();
    for attempt in 1..=RESTART_START_ATTEMPTS {
        match service.start::<&OsStr>(&[]) {
            Ok(()) => {
                return wait_for_state(&service, ServiceState::Running, RESTART_START_TIMEOUT)
            }
            Err(e) => {
                last_error = e.to_string();
                eprintln!("restart watchdog: start attempt {attempt} failed: {last_error}");
                // すでに誰かが起こしていれば成功扱い。
                if let Ok(status) = service.query_status() {
                    if status.current_state == ServiceState::Running
                        || status.current_state == ServiceState::StartPending
                    {
                        return wait_for_state(
                            &service,
                            ServiceState::Running,
                            RESTART_START_TIMEOUT,
                        );
                    }
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
    Err(ServiceError::CommandFailed {
        command: format!("start {name}"),
        exit_code: None,
        stderr: last_error,
    })
}


// ---------------------------------------------------------------------
// SCM ディスパッチャ: `recisdb-proxy --run-as-service` 経路で
// ServiceMain として動く。
// ---------------------------------------------------------------------

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceStatus as WinServiceStatus,
};
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

/// 停止要求を受けてからサーバ本体の終了を待つ上限。これを過ぎたら
/// SCM に STOPPED を報告してからプロセスを落とす。
///
/// 本番 (fukushima) で観測した不具合の再発防止:
/// 停止要求は受理されるのにサーバが終了せず、サービスが RUNNING の
/// まま「停止処理中」として SCM に滞留した。以後の制御要求はすべて
/// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (1061) で弾かれるため、
/// 自己更新後の `sc stop` → `sc start` が丸ごと不発になり、
/// 新しい exe を置いても旧イメージが動き続けていた。
///
/// 停止できないサービスは運用できない。graceful shutdown を待つのは
/// ここまで、と上限を切って必ず落とす。SQLite は WAL なので強制終了
/// でも壊れない (次回起動時にリカバリされる)。
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// STOP_PENDING を報告するときに SCM へ伝える猶予。実際の待ち上限より
/// 長めに申告しないと、SCM 側が「応答なし」と判断してしまう。
const STOP_WAIT_HINT: Duration = Duration::from_secs(45);

/// CRT の後始末を通さずにプロセスを即座に終わらせる (終了コード 0)。
///
/// `std::process::exit` はここでは使えない。停止時には
/// `Runtime::shutdown_timeout` が見切りをつけた BonDriver リーダースレッドが
/// まだ生きており、`exit` が走らせる CRT の後始末がそれらと競合して返って
/// こない。本番では SCM に STOPPED を報告したあとプロセスだけが 30 秒近く
/// residual に残り、DB と BonDriver ハンドルを掴んだまま次のインスタンスと
/// 同居していた。
///
/// ここへ来る時点で SCM には STOPPED (exit code 0) を報告済みで、ログも
/// フラッシュ済みなので、後始末を省いて落として構わない。SCM から見れば
/// 正常停止のままなので failure actions は発火しない。
///
/// kernel32 は MSVC ターゲットでは既定でリンクされるため、宣言だけで足りる。
/// どちらの関数もコールバックを取らず、巻き戻しも起こさない。
fn terminate_process_now() -> ! {
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn TerminateProcess(process: isize, exit_code: u32) -> i32;
    }
    unsafe {
        TerminateProcess(GetCurrentProcess(), 0);
    }
    // TerminateProcess が返ることはないが、型を合わせるために保険を置く。
    std::process::exit(0);
}

fn service_main_entry(_arguments: Vec<OsString>) {
    let service_name = SERVICE_NAME_STORAGE.get().cloned().unwrap_or_default();
    let should_stop = std::sync::Arc::new(AtomicBool::new(false));
    let should_stop_for_handler = std::sync::Arc::clone(&should_stop);

    // ハンドラ内から状態を報告できるよう、登録より先に置き場所を作る。
    // `register` が返すハンドルは Copy なので、登録後に埋める。
    let handle_slot: std::sync::Arc<
        std::sync::OnceLock<service_control_handler::ServiceStatusHandle>,
    > = std::sync::Arc::new(std::sync::OnceLock::new());
    let handle_for_handler = std::sync::Arc::clone(&handle_slot);

    let status_handle = match service_control_handler::register(
        &service_name,
        move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    // 二重に受けても副作用がないようにする (SCM は停止が
                    // 完了しないと同じ制御を再送してくることがある)。
                    let first = !should_stop_for_handler.swap(true, Ordering::SeqCst);
                    if first {
                        // `warn` rather than `info`: the deployed log level is
                        // usually WARN, and "why did the service stop" is
                        // exactly what an operator needs in the file. One line
                        // per stop, so it cannot become noise.
                        tracing::warn!(
                            "SCM stop control received; beginning graceful shutdown (timeout {}s)",
                            GRACEFUL_STOP_TIMEOUT.as_secs()
                        );
                    }
                    // **ここで STOP_PENDING を報告するのが SCM との契約。**
                    // 報告しないと SCM から見て状態が RUNNING のまま
                    // 停止要求だけが滞留し、以後の制御が 1061 で弾かれる。
                    if let Some(handle) = handle_for_handler.get() {
                        let _ = handle.set_service_status(WinServiceStatus {
                            service_type: ServiceType::OWN_PROCESS,
                            current_state: ServiceState::StopPending,
                            controls_accepted: ServiceControlAccept::empty(),
                            exit_code: ServiceExitCode::Win32(0),
                            checkpoint: 1,
                            wait_hint: STOP_WAIT_HINT,
                            process_id: None,
                        });
                    }
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
    let _ = handle_slot.set(status_handle);

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
        // SHUTDOWN も受ける。受けないと OS 再起動時に停止通知が来ず、
        // DB とログのフラッシュ機会がないままプロセスを落とされる。
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            0,
        );
        spawn_stop_watchdog(status_handle, std::sync::Arc::clone(&should_stop));
        // サーバ本体を実行。`should_stop` を見て graceful shutdown する
        // 責務は呼び出し元 (main.rs 側の run_server) にある。この呼び出し
        // はサーバが終了するまでブロックする。ここが戻ってこない場合は
        // 上の watchdog がプロセスごと落とす。
        body(should_stop);
        report(ServiceState::StopPending, ServiceControlAccept::empty(), 1);
    }

    SERVICE_STOPPED_CLEANLY.store(true, Ordering::SeqCst);
    report(ServiceState::Stopped, ServiceControlAccept::empty(), 0);

    // Returning from here would unwind back through `service_dispatcher::start`
    // and `main`, and in production that did not actually end the process:
    // after reporting STOPPED the old instance stayed alive alongside the one
    // the SCM had just started, with both holding the database and the
    // BonDriver handles. The blocking reader threads that `shutdown_timeout`
    // gave up on are still around and nothing can join them, so exit outright.
    //
    // SCM has already been told STOPPED, so this is a clean stop as far as the
    // service manager is concerned — failure actions do not fire.
    crate::logging::flush_log_writer();
    terminate_process_now();
}

/// 停止要求を受けたのにサーバ本体が終了しないケースの最後の砦。
///
/// 停止フラグが立ってから `GRACEFUL_STOP_TIMEOUT` を過ぎても
/// `service_main_entry` が STOPPED を報告していなければ、ここで
/// STOPPED を報告してからプロセスを終了する。
///
/// **STOPPED を報告してから終了するのが重要**: 報告せずに落ちると SCM は
/// 「予期しない終了」と見なして failure actions (RESTART) を発火させる。
/// 自己更新の再起動シーケンスと二重に起動が走り、ポートを奪い合う。
fn spawn_stop_watchdog(
    status_handle: service_control_handler::ServiceStatusHandle,
    should_stop: std::sync::Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        // 停止要求が来るまで待つ。
        while !should_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
        let deadline = std::time::Instant::now() + GRACEFUL_STOP_TIMEOUT;
        let mut checkpoint = 2u32;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            if SERVICE_STOPPED_CLEANLY.load(Ordering::SeqCst) {
                return;
            }
            // 進行中であることを SCM に伝え続ける (checkpoint を進める)。
            let _ = status_handle.set_service_status(WinServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint,
                wait_hint: STOP_WAIT_HINT,
                process_id: None,
            });
            checkpoint = checkpoint.saturating_add(1);
        }
        tracing::error!(
            "graceful shutdown did not finish within {}s; forcing process exit so the service \
             manager can restart us (this is the failure that used to leave self-update stuck)",
            GRACEFUL_STOP_TIMEOUT.as_secs()
        );
        let _ = status_handle.set_service_status(WinServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
        // `exit` skips every destructor, including the log writer's guard.
        // Without this the "why did it not stop" lines never reach the file —
        // which is exactly what made this failure so hard to diagnose.
        crate::logging::flush_log_writer();
        terminate_process_now();
    });
}

/// `service_main_entry` が最後まで到達して STOPPED を報告できたか。
/// watchdog が二重に STOPPED を報告して落とすのを防ぐ。
static SERVICE_STOPPED_CLEANLY: AtomicBool = AtomicBool::new(false);

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
