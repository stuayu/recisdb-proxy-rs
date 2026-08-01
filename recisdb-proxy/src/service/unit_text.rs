//! systemd unit ファイル / launchd plist の**文字列生成のみ**を担う純粋
//! 関数群。
//!
//! `cfg(target_os = ...)` を一切使わない (ファイル自体は
//! `service/mod.rs` で `pub(crate) mod unit_text;` として無条件にコンパイル
//! される)。実際のファイル書き込み・`systemctl`/`launchctl` 起動などの
//! OS依存の副作用は `systemd.rs` / `launchd.rs` 側の責務であり、ここでは
//! 一切行わない。こうすることで、macOS 上の `cargo test` でも
//! Linux/systemd 向けの生成結果を検証できる。

use std::path::{Path, PathBuf};

use super::{ServiceScope, ServiceSpec};

// ---------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------

/// System スコープの unit ファイルパス。
pub fn systemd_system_unit_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{name}.service"))
}

/// User スコープの unit ディレクトリ。`XDG_CONFIG_HOME` が設定されていれば
/// それを、なければ `$HOME/.config` を基準にする。
/// 環境変数の読み取り自体は呼び出し側 (`systemd.rs`) の責務とし、ここでは
/// 引数として受け取るだけにして純関数を保つ。
pub fn systemd_user_unit_dir(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    match xdg_config_home {
        Some(p) if !p.as_os_str().is_empty() => p.join("systemd/user"),
        _ => home.join(".config/systemd/user"),
    }
}

/// User スコープの unit ファイルパス。
pub fn systemd_user_unit_path(home: &Path, xdg_config_home: Option<&Path>, name: &str) -> PathBuf {
    systemd_user_unit_dir(home, xdg_config_home).join(format!("{name}.service"))
}

/// systemd の unit 文法における "C-style" クォート。スペースを含み得る
/// 引数 (設定ファイルパスなど) を `ExecStart=` に安全に埋め込むため、
/// 常にダブルクォートで囲み、`\`, `"`, `$`, backtick をエスケープする。
fn systemd_quote_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `ExecStart=` の値 (実行ファイルパス + 引数、それぞれ引用済み)。
pub fn systemd_exec_start(exe_path: &Path, args: &[String]) -> String {
    let mut parts = vec![systemd_quote_arg(&exe_path.display().to_string())];
    parts.extend(args.iter().map(|a| systemd_quote_arg(a)));
    parts.join(" ")
}

/// unit ファイル全文を生成する。`recisdb-proxy-rs.service` (手書きテンプレ)
/// を踏襲しつつ、Restart/RestartSec と scope 別の `WantedBy` を加える。
pub fn systemd_unit_body(spec: &ServiceSpec) -> String {
    let wanted_by = match spec.scope {
        ServiceScope::System => "multi-user.target",
        ServiceScope::User => "default.target",
    };
    format!(
        "[Unit]\n\
         Description={description}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         WorkingDirectory={working_dir}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        description = spec.description,
        exec_start = systemd_exec_start(&spec.exe_path, &spec.service_args()),
        working_dir = spec.working_dir.display(),
    )
}

// ---------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------

/// launchd のラベル (`local.{name}`)。System/User 双方で同じ形式を使う。
pub fn launchd_label(name: &str) -> String {
    format!("local.{name}")
}

/// plist ファイルパス。
pub fn launchd_plist_path(scope: ServiceScope, home: &Path, name: &str) -> PathBuf {
    let label = launchd_label(name);
    match scope {
        ServiceScope::System => PathBuf::from(format!("/Library/LaunchDaemons/{label}.plist")),
        ServiceScope::User => home.join(format!("Library/LaunchAgents/{label}.plist")),
    }
}

/// XMLのテキストノード/属性値として安全な形にエスケープする。
/// (パスに `&` や `<` が含まれ得るため必須。)
fn xml_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn xml_string_element(value: &str) -> String {
    format!("        <string>{}</string>\n", xml_escape(value))
}

/// plist (XML) 全文を生成する。
///
/// - `ProgramArguments` は exe_path を先頭に、続けて `spec.args` を並べる。
/// - `WorkingDirectory` / `RunAtLoad` / `KeepAlive` は仕様どおり固定。
/// - 標準出力/エラーは `working_dir/logs/service.{out,err}` に固定する
///   (ディレクトリの作成自体は `launchd.rs` の責務)。
pub fn launchd_plist_body(spec: &ServiceSpec) -> String {
    let label = launchd_label(&spec.name);

    let mut program_args = xml_string_element(&spec.exe_path.display().to_string());
    for arg in &spec.service_args() {
        program_args.push_str(&xml_string_element(arg));
    }

    let working_dir = spec.working_dir.display().to_string();
    let out_path = spec.working_dir.join("logs").join("service.out").display().to_string();
    let err_path = spec.working_dir.join("logs").join("service.err").display().to_string();

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {program_args}\
         \t</array>\n\
         \t<key>WorkingDirectory</key>\n\
         \t<string>{working_dir}</string>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{out_path}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{err_path}</string>\n\
         </dict>\n\
         </plist>\n",
        label = xml_escape(&label),
        working_dir = xml_escape(&working_dir),
        out_path = xml_escape(&out_path),
        err_path = xml_escape(&err_path),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scope: ServiceScope, args: Vec<&str>) -> ServiceSpec {
        ServiceSpec {
            name: "recisdb-proxy".to_string(),
            display_name: "recisdb-proxy".to_string(),
            description: "recisdb-proxy: BonDriver network proxy server".to_string(),
            exe_path: PathBuf::from("/opt/recisdb-proxy/recisdb-proxy"),
            working_dir: PathBuf::from("/opt/recisdb-proxy"),
            args: args.into_iter().map(String::from).collect(),
            scope,
        }
    }

    #[test]
    fn systemd_system_path() {
        assert_eq!(
            systemd_system_unit_path("recisdb-proxy"),
            PathBuf::from("/etc/systemd/system/recisdb-proxy.service")
        );
    }

    #[test]
    fn systemd_user_path_uses_xdg_config_home_when_set() {
        let home = PathBuf::from("/home/foo");
        let xdg = PathBuf::from("/home/foo/.customcfg");
        let path = systemd_user_unit_path(&home, Some(&xdg), "recisdb-proxy");
        assert_eq!(
            path,
            PathBuf::from("/home/foo/.customcfg/systemd/user/recisdb-proxy.service")
        );
    }

    #[test]
    fn systemd_user_path_falls_back_to_home_config() {
        let home = PathBuf::from("/home/foo");
        let path = systemd_user_unit_path(&home, None, "recisdb-proxy");
        assert_eq!(
            path,
            PathBuf::from("/home/foo/.config/systemd/user/recisdb-proxy.service")
        );
    }

    #[test]
    fn systemd_exec_start_quotes_args_with_spaces() {
        let exe = PathBuf::from("/opt/recisdb proxy/recisdb-proxy");
        let args = vec!["-f".to_string(), "/etc/recisdb proxy.toml".to_string()];
        let out = systemd_exec_start(&exe, &args);
        assert_eq!(
            out,
            "\"/opt/recisdb proxy/recisdb-proxy\" \"-f\" \"/etc/recisdb proxy.toml\""
        );
    }

    #[test]
    fn systemd_unit_body_system_scope_uses_multi_user_target() {
        let s = spec(ServiceScope::System, vec!["-f", "/etc/recisdb-proxy.toml"]);
        let body = systemd_unit_body(&s);
        assert!(body.contains("WantedBy=multi-user.target"));
        // 利用者指定の引数の前に隠しフラグ (--run-as-service --service-name)
        // が入る。これがサービス配下かどうかの判定材料になる
        // (`service::running_under_service_manager`)。
        assert!(body.contains(&format!(
            "ExecStart=\"/opt/recisdb-proxy/recisdb-proxy\" \"--run-as-service\" \"--service-name\" \"{}\" \"-f\" \"/etc/recisdb-proxy.toml\"",
            s.name
        )));
        assert!(body.contains("WorkingDirectory=/opt/recisdb-proxy"));
        assert!(body.contains("Restart=always"));
        assert!(body.contains("RestartSec=5"));
    }

    #[test]
    fn systemd_unit_body_user_scope_uses_default_target() {
        let s = spec(ServiceScope::User, vec![]);
        let body = systemd_unit_body(&s);
        assert!(body.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_label_format() {
        assert_eq!(launchd_label("recisdb-proxy"), "local.recisdb-proxy");
    }

    #[test]
    fn launchd_plist_path_scopes() {
        let home = PathBuf::from("/Users/foo");
        assert_eq!(
            launchd_plist_path(ServiceScope::System, &home, "recisdb-proxy"),
            PathBuf::from("/Library/LaunchDaemons/local.recisdb-proxy.plist")
        );
        assert_eq!(
            launchd_plist_path(ServiceScope::User, &home, "recisdb-proxy"),
            PathBuf::from("/Users/foo/Library/LaunchAgents/local.recisdb-proxy.plist")
        );
    }

    #[test]
    fn launchd_plist_body_escapes_xml_special_chars() {
        let mut s = spec(ServiceScope::User, vec!["-f", "/etc/a&b<c>.toml"]);
        s.working_dir = PathBuf::from("/opt/recisdb & proxy");
        let body = launchd_plist_body(&s);
        assert!(body.contains("<string>local.recisdb-proxy</string>"));
        assert!(body.contains("/etc/a&amp;b&lt;c&gt;.toml"));
        assert!(!body.contains("a&b<c>.toml")); // must not appear unescaped
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(body.contains("<true/>"));
        assert!(body.contains("<key>KeepAlive</key>"));
        assert!(body.contains("/opt/recisdb &amp; proxy/logs/service.out"));
        assert!(body.contains("/opt/recisdb &amp; proxy/logs/service.err"));
    }

    #[test]
    fn launchd_plist_body_is_well_formed_enough() {
        let s = spec(ServiceScope::System, vec![]);
        let body = launchd_plist_body(&s);
        assert!(body.starts_with("<?xml"));
        assert!(body.trim_end().ends_with("</plist>"));
        assert_eq!(body.matches("<dict>").count(), 1);
        assert_eq!(body.matches("</dict>").count(), 1);
    }
}
