//! OS サービス登録レイヤー(Phase A: コア層のみ)。
//!
//! `recisdb-proxy` を systemd (Linux) / launchd (macOS) / Windows SCM の
//! サービスとして登録・起動・停止・状態取得するための、プラットフォーム
//! 非依存の公開 API を提供する。実際のコマンド実行・ファイル生成は
//! `systemd.rs` / `launchd.rs` / `windows_scm.rs` に `cfg(target_os = ...)`
//! で分離し、ここ (`mod.rs`) はディスパッチと共通の型・バリデーションだけを
//! 持つ。
//!
//! unit ファイル/plist の**文字列生成**はさらに `unit_text.rs` へ切り出して
//! ある。あちらは `cfg` なしの純関数なので、macOS 上でも Linux/launchd 向け
//! の生成結果をテストできる。
//!
//! Web API・Web UI・setup GUI からの利用は別タスク。ここでは CLI
//! (`main.rs` の `recisdb-proxy service ...` サブコマンド) から呼ばれる
//! ことを想定している。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) mod unit_text;

#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "macos")]
mod launchd;
#[cfg(windows)]
pub mod windows_scm;

mod restart;
pub use restart::{restart_method, restart_process, restart_self, RestartMethod};

/// サービス名のデフォルト値。CLI のデフォルト引数・Web API 双方から参照される。
pub const DEFAULT_SERVICE_NAME: &str = "recisdb-proxy";

/// サービスをどのスコープに登録するか。
///
/// - `System`: OS 全体で1つ (systemd system unit / LaunchDaemon / Windows
///   サービス)。管理者権限が要る。
/// - `User`: ログインユーザー単位 (systemd --user / LaunchAgent)。Windows
///   の SCM にはユーザースコープの概念がないため `NotSupported` を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceScope {
    System,
    User,
}

/// サービス登録に必要な情報一式。呼び出し側 (CLI/将来のWeb API) が組み立てる。
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// systemctl/launchctl/sc.exe に渡すサービス名。**`sanitize_service_name`
    /// を通した値であること** (呼び出し側の責務。ここでは再検証しない)。
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub exe_path: PathBuf,
    pub working_dir: PathBuf,
    /// 例: `["-f", "/path/recisdb-proxy.toml"]`。要素ごとにOS側の引用規則で
    /// エスケープされるので、呼び出し側は生の値をそのまま渡してよい。
    pub args: Vec<String>,
    pub scope: ServiceScope,
}

/// サービスマネージャが起動するプロセスに必ず前置される隠しフラグ。
/// これが付いていることで、プロセスは自分がサービスとして動いていると
/// 確実に判断できる (環境変数ヒューリスティックは手動デーモン起動を
/// 誤検出しうる — `running_under_service_manager` 参照)。
pub const RUN_AS_SERVICE_FLAG: &str = "--run-as-service";
/// 登録名を自プロセスに伝える隠しフラグ。値は次の引数。
pub const SERVICE_NAME_FLAG: &str = "--service-name";
/// 作業ディレクトリを明示する隠しフラグ (Windows SCM は
/// WorkingDirectory を持たないため)。値は次の引数。
pub const SERVICE_WORKDIR_FLAG: &str = "--service-workdir";

impl ServiceSpec {
    /// サービス定義 (systemd unit / launchd plist / SCM launch_arguments)
    /// に書く実際の引数列。利用者が指定した `args` の前に隠しフラグを
    /// 置く。
    pub fn service_args(&self) -> Vec<String> {
        let mut out = vec![
            RUN_AS_SERVICE_FLAG.to_string(),
            SERVICE_NAME_FLAG.to_string(),
            self.name.clone(),
        ];
        out.extend(self.args.iter().cloned());
        out
    }
}

/// 現在のサービス登録状態。取得できない項目は保守的な既定値 (false /
/// None) を入れて返す。呼び出し側が panic することはない。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServiceStatus {
    /// このOSでサービス登録そのものに対応しているか。
    pub supported: bool,
    /// "systemd" | "launchd" | "windows-scm" | "unsupported"
    pub manager: String,
    pub name: String,
    pub scope: ServiceScope,
    pub installed: bool,
    pub running: bool,
    /// 自動起動が有効か。取得できなければ `installed` と同値。
    pub enabled: bool,
    /// systemctl の ActiveState など、生の診断情報。
    pub detail: Option<String>,
}

impl ServiceStatus {
    /// 「未サポート」を表す既定値。
    fn unsupported(name: &str, scope: ServiceScope) -> Self {
        Self {
            supported: false,
            manager: "unsupported".to_string(),
            name: name.to_string(),
            scope,
            installed: false,
            running: false,
            enabled: false,
            detail: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("service management is not supported on this platform/scope")]
    NotSupported,
    #[error("permission denied (try re-running with administrator/root privileges)")]
    PermissionDenied,
    #[error("command failed: {command} (exit={exit_code:?}): {stderr}")]
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid service name {raw:?}: {reason}")]
    InvalidName { raw: String, reason: String },
}

/// サービス名として使ってよい文字だけかを検証し、正規化した名前を返す。
///
/// **セキュリティ上重要**: サービス名は `systemctl`/`launchctl`/`sc.exe`
/// の引数や unit/plist ファイルパスにそのまま埋め込まれる。空白・シェル
/// メタ文字・パス区切り・`..` を一切通さない。許可文字は
/// `[A-Za-z0-9._-]` のみ、長さ 1..=64、先頭は英数字。
pub fn sanitize_service_name(raw: &str) -> Result<String, ServiceError> {
    let invalid = |reason: &str| ServiceError::InvalidName {
        raw: raw.to_string(),
        reason: reason.to_string(),
    };

    if raw.is_empty() || raw.chars().count() > 64 {
        return Err(invalid("length must be between 1 and 64 characters"));
    }
    let first = raw.chars().next().expect("checked non-empty above");
    if !first.is_ascii_alphanumeric() {
        return Err(invalid("must start with an ASCII letter or digit"));
    }
    if raw.contains("..") {
        return Err(invalid("must not contain '..'"));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(invalid(
            "only ASCII letters, digits, '.', '_', '-' are allowed",
        ));
    }
    Ok(raw.to_string())
}

/// 妥当なデフォルト値で `ServiceSpec` を組み立てるヘルパー。
///
/// `name` は事前に `sanitize_service_name` を通しておくこと (ここでは
/// 再検証しない — 呼び出し側で早期にエラー表示させるため)。
pub fn default_spec(
    name: String,
    scope: ServiceScope,
    exe_path: PathBuf,
    working_dir: PathBuf,
    args: Vec<String>,
) -> ServiceSpec {
    ServiceSpec {
        display_name: format!("recisdb-proxy ({name})"),
        description: "recisdb-proxy: BonDriver network proxy server".to_string(),
        name,
        exe_path,
        working_dir,
        args,
        scope,
    }
}

/// このOSでサービス登録機能そのものをビルドしているか (荒い判定)。
/// スコープ単位の細かい可否 (例: Windows の User スコープ) は各操作の
/// `Err(ServiceError::NotSupported)` で表現する。
pub const fn is_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos", windows))
}

// --- Windows専用: サービスとして起動されたかを示すプロセス内フラグ ---
//
// Unix (systemd/launchd) は登録した実行ファイルを直接 exec するので、
// プロセスからは「サービスかどうか」を環境変数から検出できる。Windows の
// SCM はサービスプロセスを `--run-as-service` フラグ付きで起動する
// (`main.rs` 参照) ので、そのフラグを受けたらこのグローバルを立てる。
static RUNNING_AS_SERVICE: AtomicBool = AtomicBool::new(false);

/// 自分がどのサービス名で登録されているか (`--service-name` 由来)。
/// Windows で自分自身を `sc stop`/`sc start` し直すのに要る
/// (`restart.rs`)。Unix でも記録はするが再起動には使わない。
static SELF_SERVICE_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `main.rs` が `--run-as-service` を検出したときに呼ぶ。
///
/// `name` は `--service-name` の値 (SCM が起動した場合のみ渡される)。
pub fn mark_running_as_service(name: Option<&str>) {
    RUNNING_AS_SERVICE.store(true, Ordering::Relaxed);
    if let Some(name) = name {
        let _ = SELF_SERVICE_NAME.set(name.to_string());
    }
}

/// 自プロセスが登録されているサービス名 (分かる場合)。
pub fn current_service_name() -> Option<String> {
    SELF_SERVICE_NAME.get().cloned()
}

/// 現在のプロセスがOSのサービス管理下で動いているか。
///
/// 第一の判定材料は `--run-as-service`(`ServiceSpec::service_args` が
/// 必ず前置し、`main.rs` が `mark_running_as_service` を呼ぶ)。これが
/// 立っていない場合のみ、旧バージョンが書いたサービス定義のために環境
/// 変数ヒューリスティックを見る:
///
/// - Linux: systemd が起動したプロセスに設定する `INVOCATION_ID` または
///   `JOURNAL_STREAM` の存在。
/// - macOS: `XPC_SERVICE_NAME` が設定されていて `"0"` でない場合。
///   **親PIDが1かどうかは見ない** — `&` でバックグラウンド起動して親
///   シェルが終了しただけのプロセスも ppid=1 になり、「再起動」が
///   ただの停止になってしまうため。
/// - Windows: SCM 経路は必ず `--run-as-service` を伴うので、追加の
///   ヒューリスティックは無い。
pub fn running_under_service_manager() -> bool {
    if RUNNING_AS_SERVICE.load(Ordering::Relaxed) {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("INVOCATION_ID").is_some() || std::env::var_os("JOURNAL_STREAM").is_some()
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("XPC_SERVICE_NAME")
            .map(|v| v != "0")
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

// --- ディスパッチ ---
//
// 各操作は対応するプラットフォームモジュールへ委譲する。非対応OSでは
// `NotSupported` (status は「未対応」を表す構造体) を返す。

#[cfg(target_os = "linux")]
pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    systemd::install(spec)
}
#[cfg(target_os = "macos")]
pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    launchd::install(spec)
}
#[cfg(windows)]
pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    windows_scm::install(spec)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn install(_spec: &ServiceSpec) -> Result<(), ServiceError> {
    Err(ServiceError::NotSupported)
}

#[cfg(target_os = "linux")]
pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    systemd::uninstall(name, scope)
}
#[cfg(target_os = "macos")]
pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    launchd::uninstall(name, scope)
}
#[cfg(windows)]
pub fn uninstall(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    windows_scm::uninstall(name, scope)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn uninstall(_name: &str, _scope: ServiceScope) -> Result<(), ServiceError> {
    Err(ServiceError::NotSupported)
}

#[cfg(target_os = "linux")]
pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    systemd::start(name, scope)
}
#[cfg(target_os = "macos")]
pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    launchd::start(name, scope)
}
#[cfg(windows)]
pub fn start(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    windows_scm::start(name, scope)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn start(_name: &str, _scope: ServiceScope) -> Result<(), ServiceError> {
    Err(ServiceError::NotSupported)
}

#[cfg(target_os = "linux")]
pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    systemd::stop(name, scope)
}
#[cfg(target_os = "macos")]
pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    launchd::stop(name, scope)
}
#[cfg(windows)]
pub fn stop(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    windows_scm::stop(name, scope)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn stop(_name: &str, _scope: ServiceScope) -> Result<(), ServiceError> {
    Err(ServiceError::NotSupported)
}

#[cfg(target_os = "linux")]
pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    systemd::restart(name, scope)
}
#[cfg(target_os = "macos")]
pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    launchd::restart(name, scope)
}
#[cfg(windows)]
pub fn restart(name: &str, scope: ServiceScope) -> Result<(), ServiceError> {
    windows_scm::restart(name, scope)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn restart(_name: &str, _scope: ServiceScope) -> Result<(), ServiceError> {
    Err(ServiceError::NotSupported)
}

#[cfg(target_os = "linux")]
pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    systemd::status(name, scope)
}
#[cfg(target_os = "macos")]
pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    launchd::status(name, scope)
}
#[cfg(windows)]
pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    windows_scm::status(name, scope)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn status(name: &str, scope: ServiceScope) -> ServiceStatus {
    ServiceStatus::unsupported(name, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_args_prefix_hidden_flags_before_user_args() {
        let spec = default_spec(
            "my-proxy".to_string(),
            ServiceScope::System,
            PathBuf::from("/opt/recisdb-proxy/recisdb-proxy"),
            PathBuf::from("/opt/recisdb-proxy"),
            vec!["-f".to_string(), "/etc/recisdb-proxy.toml".to_string()],
        );
        assert_eq!(
            spec.service_args(),
            vec![
                "--run-as-service",
                "--service-name",
                "my-proxy",
                "-f",
                "/etc/recisdb-proxy.toml",
            ]
        );
    }

    #[test]
    fn restart_method_without_service_marker_is_exec_self() {
        // テストプロセスはサービス配下ではないので、「終了してマネージャに
        // 起こし直してもらう」方式を選んではならない (= ただの停止になる)。
        assert_eq!(restart_method(), RestartMethod::ExecSelf);
    }

    #[test]
    fn sanitize_accepts_normal_names() {
        assert_eq!(sanitize_service_name("recisdb-proxy").unwrap(), "recisdb-proxy");
        assert_eq!(sanitize_service_name("a").unwrap(), "a");
        assert_eq!(sanitize_service_name("a.b_c-9").unwrap(), "a.b_c-9");
        assert_eq!(sanitize_service_name(&"a".repeat(64)).unwrap().len(), 64);
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(matches!(
            sanitize_service_name(""),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_rejects_too_long() {
        let raw = "a".repeat(65);
        assert!(matches!(
            sanitize_service_name(&raw),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(matches!(
            sanitize_service_name("../etc"),
            Err(ServiceError::InvalidName { .. })
        ));
        assert!(matches!(
            sanitize_service_name("a..b"),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_rejects_whitespace() {
        assert!(matches!(
            sanitize_service_name("a b"),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_rejects_shell_metacharacters() {
        assert!(matches!(
            sanitize_service_name("a;rm -rf"),
            Err(ServiceError::InvalidName { .. })
        ));
        assert!(matches!(
            sanitize_service_name("a$(x)"),
            Err(ServiceError::InvalidName { .. })
        ));
        assert!(matches!(
            sanitize_service_name("a&b"),
            Err(ServiceError::InvalidName { .. })
        ));
        assert!(matches!(
            sanitize_service_name("a/b"),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_rejects_leading_non_alnum() {
        assert!(matches!(
            sanitize_service_name("-abc"),
            Err(ServiceError::InvalidName { .. })
        ));
        assert!(matches!(
            sanitize_service_name(".abc"),
            Err(ServiceError::InvalidName { .. })
        ));
    }

    #[test]
    fn default_spec_fills_reasonable_defaults() {
        let spec = default_spec(
            "recisdb-proxy".to_string(),
            ServiceScope::System,
            PathBuf::from("/usr/local/bin/recisdb-proxy"),
            PathBuf::from("/var/lib/recisdb-proxy"),
            vec!["-f".to_string(), "/etc/recisdb-proxy.toml".to_string()],
        );
        assert_eq!(spec.name, "recisdb-proxy");
        assert!(spec.display_name.contains("recisdb-proxy"));
        assert!(!spec.description.is_empty());
    }
}
