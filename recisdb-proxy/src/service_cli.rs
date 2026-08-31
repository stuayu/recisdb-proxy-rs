//! `recisdb-proxy service <action>` サブコマンドの実装 (バイナリ専用)。
//!
//! OS ごとのサービス登録そのものは `recisdb_proxy::service` が担当する。
//! ここはその薄いフロントエンドで、
//!
//! - サービス名のバリデーション (`sanitize_service_name`)
//! - 実行ファイルパス・作業ディレクトリ・設定ファイルパスの解決
//! - 人間可読なメッセージと終了コード
//!
//! だけを行う。ロギング初期化より前に呼ばれるので `log!` ではなく
//! `println!`/`eprintln!` を使う。

use std::path::{Path, PathBuf};

use clap::Subcommand;

use recisdb_proxy::service::{
    self, ServiceError, ServiceScope, ServiceStatus, DEFAULT_SERVICE_NAME,
};

#[derive(Subcommand, Debug, Clone)]
pub enum ServiceAction {
    /// OSサービスとして登録し、自動起動を有効にして開始する
    Install {
        /// サービス名 (英数字と `.` `_` `-` のみ、64文字以内)
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        /// システム全体ではなくログインユーザー単位で登録する
        /// (Linux: `systemctl --user` / macOS: LaunchAgents。Windows非対応)
        #[arg(long)]
        user: bool,
        /// サービスに渡す設定ファイルパス
        /// (省略時: グローバルな `-f/--config`、それも無ければ
        ///  作業ディレクトリ内の `recisdb-proxy.toml`)
        #[arg(long)]
        config: Option<PathBuf>,
        /// サービスの作業ディレクトリ (省略時: 実行ファイルのあるディレクトリ)
        #[arg(long)]
        working_dir: Option<PathBuf>,
        /// サーバに追加で渡す引数 (`-- --web-listen 0.0.0.0:40080` のように書く)
        #[arg(last = true)]
        extra_args: Vec<String>,
    },
    /// サービスを停止・無効化して登録を削除する
    Uninstall {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        #[arg(long)]
        user: bool,
    },
    /// サービスを開始する
    Start {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        #[arg(long)]
        user: bool,
    },
    /// サービスを停止する
    Stop {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        #[arg(long)]
        user: bool,
    },
    /// サービスを再起動する
    Restart {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        #[arg(long)]
        user: bool,
    },
    /// サービスの登録状況・稼働状況を表示する
    Status {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        #[arg(long)]
        user: bool,
    },
}

fn scope_of(user: bool) -> ServiceScope {
    if user {
        ServiceScope::User
    } else {
        ServiceScope::System
    }
}

fn scope_label(scope: ServiceScope) -> &'static str {
    match scope {
        ServiceScope::System => "システム",
        ServiceScope::User => "ユーザー",
    }
}

/// 実行ファイルのあるディレクトリ。取得できなければカレントディレクトリ。
fn exe_dir(exe_path: &Path) -> PathBuf {
    exe_path
        .parent()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// サービス定義には必ず絶対パスを書く (systemd/launchd/SCM はいずれも
/// 相対パスを解決する保証がない)。
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// エラーを利用者向けの案内に変換する。
fn explain(err: &ServiceError, scope: ServiceScope) -> String {
    match err {
        ServiceError::PermissionDenied => {
            let hint = if cfg!(windows) {
                "管理者として実行したコマンドプロンプト/PowerShell から実行してください。"
            } else {
                "`sudo` を付けて実行するか、`--user` でユーザー単位のサービスとして登録してください。"
            };
            format!(
                "権限が不足しています ({}スコープ)。{}",
                scope_label(scope),
                hint
            )
        }
        ServiceError::NotSupported => {
            if cfg!(windows) && scope == ServiceScope::User {
                "Windows はユーザー単位のサービスに対応していません (`--user` を外してください)。"
                    .to_string()
            } else {
                "このプラットフォームではサービス登録に対応していません。".to_string()
            }
        }
        other => other.to_string(),
    }
}

fn print_status(status: &ServiceStatus) {
    println!("サービス名 : {}", status.name);
    println!("スコープ   : {}", scope_label(status.scope));
    println!("管理方式   : {}", status.manager);
    println!(
        "登録済み   : {}",
        if status.installed {
            "はい"
        } else {
            "いいえ"
        }
    );
    println!(
        "稼働中     : {}",
        if status.running {
            "はい"
        } else {
            "いいえ"
        }
    );
    println!(
        "自動起動   : {}",
        if status.enabled { "有効" } else { "無効" }
    );
    if let Some(detail) = &status.detail {
        println!("詳細       : {}", detail);
    }
}

/// サブコマンドを実行し、プロセス終了コードを返す。
///
/// `global_config` は `recisdb-proxy -f <path> service install` のように
/// グローバル側で指定された設定ファイルパス。
pub fn run(action: &ServiceAction, global_config: Option<&Path>) -> i32 {
    if !service::is_supported() {
        eprintln!("このプラットフォームではサービス登録に対応していません。");
        return 1;
    }

    match action {
        ServiceAction::Install {
            name,
            user,
            config,
            working_dir,
            extra_args,
        } => install(
            name,
            *user,
            config.as_deref(),
            working_dir.as_deref(),
            extra_args,
            global_config,
        ),
        ServiceAction::Uninstall { name, user } => {
            simple(name, *user, "登録を削除", service::uninstall)
        }
        ServiceAction::Start { name, user } => simple(name, *user, "開始", service::start),
        ServiceAction::Stop { name, user } => simple(name, *user, "停止", service::stop),
        ServiceAction::Restart { name, user } => simple(name, *user, "再起動", service::restart),
        ServiceAction::Status { name, user } => status(name, *user),
    }
}

fn simple(
    name: &str,
    user: bool,
    verb: &str,
    op: fn(&str, ServiceScope) -> Result<(), ServiceError>,
) -> i32 {
    let scope = scope_of(user);
    let name = match service::sanitize_service_name(name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    match op(&name, scope) {
        Ok(()) => {
            println!("サービス `{}` を{}しました。", name, verb);
            0
        }
        Err(e) => {
            eprintln!(
                "サービス `{}` の{}に失敗しました: {}",
                name,
                verb,
                explain(&e, scope)
            );
            1
        }
    }
}

fn status(name: &str, user: bool) -> i32 {
    let scope = scope_of(user);
    let name = match service::sanitize_service_name(name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    // 未登録でもエラー扱いにはしない (状態を出して 0 で終わる)。
    print_status(&service::status(&name, scope));
    0
}

fn install(
    name: &str,
    user: bool,
    action_config: Option<&Path>,
    working_dir: Option<&Path>,
    extra_args: &[String],
    global_config: Option<&Path>,
) -> i32 {
    let scope = scope_of(user);
    let name = match service::sanitize_service_name(name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };

    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("実行ファイルのパスを取得できませんでした: {}", e);
            return 1;
        }
    };
    let working_dir = working_dir
        .map(absolutize)
        .unwrap_or_else(|| exe_dir(&exe_path));

    // 設定ファイル: 明示指定 > グローバル `-f` > 作業ディレクトリ内の既定名。
    // どれも無ければ引数を付けない (サーバ側の CWD 自動検出に任せる)。
    let config_path = action_config.or(global_config).map(absolutize).or_else(|| {
        let candidate = working_dir.join("recisdb-proxy.toml");
        candidate.exists().then_some(candidate)
    });

    let mut args: Vec<String> = Vec::new();
    if let Some(config_path) = &config_path {
        args.push("-f".to_string());
        args.push(config_path.to_string_lossy().into_owned());
    }
    args.extend(extra_args.iter().cloned());

    let spec = service::default_spec(name.clone(), scope, exe_path, working_dir, args);

    println!("サービスを登録します:");
    println!("  名前            : {}", spec.name);
    println!("  スコープ        : {}", scope_label(scope));
    println!("  実行ファイル    : {}", spec.exe_path.display());
    println!("  作業ディレクトリ: {}", spec.working_dir.display());
    match &config_path {
        Some(p) => println!("  設定ファイル    : {}", p.display()),
        None => println!("  設定ファイル    : (未指定 — 作業ディレクトリから自動検出)"),
    }

    match service::install(&spec) {
        Ok(()) => {
            println!("サービス `{}` を登録し、開始しました。", spec.name);
            println!(
                "状態は `recisdb-proxy service status --name {}{}` で確認できます。",
                spec.name,
                if user { " --user" } else { "" }
            );
            0
        }
        Err(e) => {
            eprintln!("サービスの登録に失敗しました: {}", explain(&e, scope));
            1
        }
    }
}
