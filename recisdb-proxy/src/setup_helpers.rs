//! recisdb-proxy セットアップウィザード共通ロジック
//!
//! `bin/setup_gui.rs` (GUIウィザード) から使われる、UIを持たない純粋なロジック
//! (チューナー検出・設定ファイル生成・DB登録)。GUIに依存しないためテストしやすい。

use crate::database::Database;
use std::path::{Path, PathBuf};

// =============================================================================
// チューナー定義
// =============================================================================

/// 既知のチューナーデバイス情報
#[allow(dead_code)]
pub struct KnownTuner {
    /// デバイス名 (人間向けの表示名)
    pub name: &'static str,
    /// USB Vendor ID
    pub usb_vendor_id: u16,
    /// USB Product ID
    pub usb_product_id: u16,
    /// グループ名 (同系統チューナーの統合用)
    pub group_name: &'static str,
    /// 地上波対応数
    pub terrestrial_count: i32,
    /// BS/CS (衛星) 対応数
    pub satellite_count: i32,
    /// BonDriverのダウンロードURL (後から設定)
    pub bondriver_url: &'static str,
    /// BonDriverのDLLファイル名パターン (Windows)
    pub bondriver_dll_pattern: &'static str,
    /// Linuxデバイスパスのパターン
    pub linux_device_pattern: &'static str,
}

/// 既知のチューナーデバイス一覧
/// NOTE: bondriver_url は後から正式なものを設定してください
#[allow(dead_code)]
pub const KNOWN_TUNERS: &[KnownTuner] = &[
    KnownTuner {
        name: "PLEX PX-MLT5PE",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x084e,
        group_name: "PX-MLT",
        terrestrial_count: 5,
        satellite_count: 5,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-MLT{n}.dll",
        linux_device_pattern: "/dev/pxmlt{n}video{i}",
    },
    KnownTuner {
        name: "PLEX PX-MLT8PE",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x0850,
        group_name: "PX-MLT",
        terrestrial_count: 8,
        satellite_count: 8,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-MLT{n}.dll",
        linux_device_pattern: "/dev/pxmlt{n}video{i}",
    },
    KnownTuner {
        name: "PLEX PX-Q3U4",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x083f,
        group_name: "PX-Q3U4",
        terrestrial_count: 4,
        satellite_count: 4,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-Q3U4_{band}{n}.dll",
        linux_device_pattern: "/dev/pxq3u4video{i}",
    },
    KnownTuner {
        name: "PLEX PX-W3U4",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x083e,
        group_name: "PX-W3U4",
        terrestrial_count: 2,
        satellite_count: 2,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-W3U4_{band}{n}.dll",
        linux_device_pattern: "/dev/pxw3u4video{i}",
    },
    KnownTuner {
        name: "PLEX PX-S1UD",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x003b,
        group_name: "PX-S1UD",
        terrestrial_count: 1,
        satellite_count: 0,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-S1UD.dll",
        linux_device_pattern: "/dev/pxs1udvideo{i}",
    },
    KnownTuner {
        name: "PLEX PX-Q1UD",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x004b,
        group_name: "PX-Q1UD",
        terrestrial_count: 4,
        satellite_count: 0,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PX-Q1UD_{n}.dll",
        linux_device_pattern: "/dev/pxq1udvideo{i}",
    },
    KnownTuner {
        name: "e-Better DTV02A-1T1S-U (MyGica S270)",
        usb_vendor_id: 0x0511,
        usb_product_id: 0x004c,
        group_name: "DTV02A",
        terrestrial_count: 1,
        satellite_count: 1,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_DTV02A_{band}.dll",
        linux_device_pattern: "/dev/isdb{type}{i}",
    },
    KnownTuner {
        name: "Earthsoft PT3",
        usb_vendor_id: 0x0000, // PCIeデバイス (USBではない)
        usb_product_id: 0x0000,
        group_name: "PT3",
        terrestrial_count: 2,
        satellite_count: 2,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PT3-{band}{n}.dll",
        linux_device_pattern: "/dev/pt3video{i}",
    },
    KnownTuner {
        name: "Earthsoft PT1/PT2",
        usb_vendor_id: 0x0000, // PCIeデバイス (USBではない)
        usb_product_id: 0x0000,
        group_name: "PT",
        terrestrial_count: 2,
        satellite_count: 2,
        bondriver_url: "", // TODO: 後から設定
        bondriver_dll_pattern: "BonDriver_PT-{band}{n}.dll",
        linux_device_pattern: "/dev/pt1video{i}",
    },
];

/// 検出されたチューナーデバイスの情報
#[derive(Debug, Clone)]
pub struct DetectedTuner {
    /// チューナー名 (画面表示用)
    pub name: String,
    /// デバイスパスのリスト
    pub device_paths: Vec<String>,
    /// グループ名
    pub group_name: String,
    /// 地上波チューナー数
    pub terrestrial_count: i32,
    /// 衛星チューナー数
    pub satellite_count: i32,
    /// BonDriverのダウンロードURL
    pub bondriver_url: String,
    /// px4_drv for WinUSB で自動インストール可能な機種の場合、その USB PID。
    /// `device_paths` が空(=まだBonDriver/ドライバが入っていない)状態でも
    /// USBデバイスとして検出できた場合にセットされる。
    pub px4_model_pid: Option<u16>,
    /// USBデバイス列挙で数えた、同一機種の接続台数。px4_drv for WinUSB は
    /// 同一機種を複数台挿しても1つのBonDriverが台数ぶんの space を自動で
    /// 公開するため、[`register_tuners_to_db`] がこの値を `max_instances` に
    /// 反映する(`Some`の場合は `terrestrial_count + satellite_count` より優先)。
    pub px4_device_count: Option<i32>,
}

// =============================================================================
// チューナー検出
// =============================================================================

/// Linuxでのチューナーデバイス検出
#[cfg(target_os = "linux")]
fn detect_tuners_linux() -> Vec<DetectedTuner> {
    let mut detected = Vec::new();

    // /dev/ 以下のチューナーデバイスファイルを検索
    let tuner_patterns = [
        ("pt3video", "Earthsoft PT3", "PT3"),
        ("pt1video", "Earthsoft PT1/PT2", "PT"),
        ("pxmlt", "PLEX PX-MLT", "PX-MLT"),
        ("pxq3u4video", "PLEX PX-Q3U4", "PX-Q3U4"),
        ("pxw3u4video", "PLEX PX-W3U4", "PX-W3U4"),
        ("pxs1udvideo", "PLEX PX-S1UD", "PX-S1UD"),
        ("pxq1udvideo", "PLEX PX-Q1UD", "PX-Q1UD"),
        ("isdb", "ISDB チューナー", "ISDB"),
    ];

    // DVBデバイスの検出
    if let Ok(entries) = std::fs::read_dir("/dev/dvb") {
        let mut adapters = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("adapter") {
                    let adapter_path = entry.path();
                    // frontend の数を数える
                    if let Ok(sub_entries) = std::fs::read_dir(&adapter_path) {
                        for sub in sub_entries.flatten() {
                            if let Some(sub_name) = sub.file_name().to_str() {
                                if sub_name.starts_with("frontend") {
                                    adapters.push(format!("/dev/dvb/{}/{}", name, sub_name));
                                }
                            }
                        }
                    }
                }
            }
        }
        if !adapters.is_empty() {
            detected.push(DetectedTuner {
                name: format!("DVB デバイス ({}個検出)", adapters.len()),
                device_paths: adapters,
                group_name: "DVB".to_string(),
                terrestrial_count: 0, // DVBでは不明
                satellite_count: 0,
                bondriver_url: String::new(),
                px4_model_pid: None,
                px4_device_count: None,
            });
        }
    }

    // キャラクターデバイスの検出
    if let Ok(entries) = std::fs::read_dir("/dev") {
        let dev_names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        for (pattern, name, group) in &tuner_patterns {
            let matching: Vec<String> = dev_names
                .iter()
                .filter(|n| n.starts_with(pattern))
                .map(|n| format!("/dev/{}", n))
                .collect();

            if !matching.is_empty() {
                // 既知チューナーの情報を取得
                let known = KNOWN_TUNERS.iter().find(|k| k.group_name == *group);

                detected.push(DetectedTuner {
                    name: format!("{} ({}デバイス検出)", name, matching.len()),
                    device_paths: matching,
                    group_name: group.to_string(),
                    terrestrial_count: known.map_or(0, |k| k.terrestrial_count),
                    satellite_count: known.map_or(0, |k| k.satellite_count),
                    bondriver_url: known.map_or(String::new(), |k| k.bondriver_url.to_string()),
                    px4_model_pid: None,
                    px4_device_count: None,
                });
            }
        }
    }

    detected
}

/// Windowsでのチューナーデバイス検出 (BonDriver DLLの検索)
///
/// `install_dir` はGUIで選択されたインストール先フォルダ(絶対パス)。
/// px4_installer が実際にBonDriverを配置するのはここなので、検索の
/// 最優先パスとする。カレントディレクトリ相対のパスもフォールバックとして
/// 残してあるが、これは「セットアップウィザードを使わず手動でDLLを配置
/// した」ような後方互換のためのもの。
#[cfg(target_os = "windows")]
fn detect_tuners_windows(install_dir: &Path) -> Vec<DetectedTuner> {
    let mut detected = Vec::new();

    // BonDriver DLLの検索パス候補 (GUIで指定されたインストール先を優先)
    let search_dirs = [
        install_dir.to_path_buf(),
        install_dir.join("BonDriver"),
        PathBuf::from("."),
        PathBuf::from("BonDriver"),
        // カレントディレクトリの親
        PathBuf::from("..\\BonDriver"),
    ];

    // 一般的なBonDriver DLLのパターン
    let dll_patterns = [
        ("BonDriver_PX-MLT", "PLEX PX-MLT", "PX-MLT"),
        ("BonDriver_PX-Q3U4", "PLEX PX-Q3U4", "PX-Q3U4"),
        ("BonDriver_PX-W3U4", "PLEX PX-W3U4", "PX-W3U4"),
        ("BonDriver_PX-S1UD", "PLEX PX-S1UD", "PX-S1UD"),
        ("BonDriver_PX-Q1UD", "PLEX PX-Q1UD", "PX-Q1UD"),
        ("BonDriver_PT3", "Earthsoft PT3", "PT3"),
        ("BonDriver_PT-", "Earthsoft PT1/PT2", "PT"),
    ];

    for dir in &search_dirs {
        if !dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            let dll_files: Vec<String> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.to_lowercase().ends_with(".dll")
                        && name.to_lowercase().starts_with("bondriver")
                    {
                        Some(
                            e.path()
                                .canonicalize()
                                .unwrap_or_else(|_| e.path())
                                .to_string_lossy()
                                .to_string(),
                        )
                    } else {
                        None
                    }
                })
                .collect();

            for (pattern, name, group) in &dll_patterns {
                let matching: Vec<String> = dll_files
                    .iter()
                    .filter(|p| {
                        Path::new(p)
                            .file_name()
                            .map_or(false, |n| n.to_string_lossy().starts_with(pattern))
                    })
                    .cloned()
                    .collect();

                if !matching.is_empty() {
                    // 重複チェック
                    let already_found = detected
                        .iter()
                        .any(|d: &DetectedTuner| d.group_name == *group);
                    if already_found {
                        continue;
                    }

                    let known = KNOWN_TUNERS.iter().find(|k| k.group_name == *group);

                    detected.push(DetectedTuner {
                        name: format!("{} ({}個のDLL検出)", name, matching.len()),
                        device_paths: matching,
                        group_name: group.to_string(),
                        terrestrial_count: known.map_or(0, |k| k.terrestrial_count),
                        satellite_count: known.map_or(0, |k| k.satellite_count),
                        bondriver_url: known
                            .map_or(String::new(), |k| k.bondriver_url.to_string()),
                        px4_model_pid: None,
                        px4_device_count: None,
                    });
                }
            }
        }
    }

    // USBデバイスとしての検出 (px4_drv for WinUSB 対応機種)。BonDriver DLLの
    // 有無に関わらず、接続されているだけで検出でき、接続台数も分かる。
    // 上のDLL検索は px4_drv 系のフォルダ構成(px4_installer が配置する
    // `BonDriver/<フォルダ名>/*.dll`)を認識しないため、既にインストール済み
    // でも「ドライバ未インストール」と誤判定してしまう。ここで
    // find_staged_px4_bondriver により専用の配置場所を確認し、正しい
    // device_paths を埋める。
    for (model, count) in crate::px4_installer::detect_connected_px4_devices() {
        if let Some(existing) = detected.iter_mut().find(|d| d.group_name == model.bondriver_folder) {
            existing.px4_device_count = Some(count as i32);
            continue;
        }

        let staged_paths = find_staged_px4_bondriver(model, install_dir);
        let installed = !staged_paths.is_empty();

        detected.push(DetectedTuner {
            name: if installed {
                format!("{} x{count}", model.label)
            } else {
                format!("{} x{count} (ドライバ未インストール)", model.label)
            },
            device_paths: staged_paths,
            group_name: model.bondriver_folder.to_string(),
            terrestrial_count: 0,
            satellite_count: 0,
            bondriver_url: String::new(),
            px4_model_pid: Some(model.usb_pid),
            px4_device_count: Some(count as i32),
        });
    }

    detected
}

/// [`crate::px4_installer::download_install_and_stage`] が配置した
/// BonDriver一式が既に存在するかを、そのステージング先の規則
/// (`<検索ルート>\BonDriver\<bondriver_folder>\<dll名>`)に従って探す。
/// `install_dir` (GUIで指定されたインストール先) を最優先で確認し、
/// カレントディレクトリ相対のパスは後方互換のフォールバックとする。
/// 見つかった場合は絶対パスのリストを返す(空なら未インストール)。
#[cfg(target_os = "windows")]
fn find_staged_px4_bondriver(model: &crate::px4_installer::Px4Model, install_dir: &Path) -> Vec<String> {
    find_staged_px4_bondriver_in(
        model,
        &[install_dir.to_path_buf(), PathBuf::from("."), PathBuf::from("..")],
    )
}

#[cfg(target_os = "windows")]
fn find_staged_px4_bondriver_in(
    model: &crate::px4_installer::Px4Model,
    staged_roots: &[PathBuf],
) -> Vec<String> {
    for root in staged_roots {
        let candidate_dir = root.join("BonDriver").join(model.bondriver_folder);
        if !candidate_dir.is_dir() {
            continue;
        }

        let paths: Vec<String> = model
            .dll_names
            .iter()
            .filter_map(|dll_name| {
                let p = candidate_dir.join(dll_name);
                p.exists().then(|| {
                    p.canonicalize()
                        .unwrap_or(p)
                        .to_string_lossy()
                        .to_string()
                })
            })
            .collect();

        if !paths.is_empty() {
            return paths;
        }
    }

    Vec::new()
}

/// チューナーを検出する。時間がかかることがあるため、GUIから呼ぶ場合はワーカー
/// スレッド上で実行すること。
///
/// `install_dir` はGUIで指定されたインストール先フォルダ(絶対パス)。
/// Windows版のpx4_drv対応BonDriver検索はここを優先的に見に行く
/// (`detect_tuners_windows` 参照)。Linuxではデバイスパスのみで検出する
/// ため使用しない。
pub fn detect_tuners(install_dir: &Path) -> Vec<DetectedTuner> {
    #[cfg(target_os = "linux")]
    {
        let _ = install_dir;
        detect_tuners_linux()
    }
    #[cfg(target_os = "windows")]
    {
        detect_tuners_windows(install_dir)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = install_dir;
        Vec::new()
    }
}

// =============================================================================
// 設定ファイル生成
// =============================================================================

/// TOMLの基本文字列(ダブルクォート)にそのまま埋め込めるよう、バックスラッシュを
/// エスケープする。Windowsの絶対パス(`C:\DTV\...`)をそのまま埋め込むと、
/// `\D` が不正なエスケープシーケンスとして扱われて設定ファイルが壊れ、
/// recisdb-proxy本体が起動できなくなる(`\r`のように偶然有効なエスケープに
/// 化けて経路が化けるケースもある)。
fn escape_toml_basic_string(s: &str) -> String {
    s.replace('\\', "\\\\")
}

/// 実際に配布している `recisdb-proxy.toml.example` そのものをテンプレートとして
/// 埋め込む(ビルド時に取り込まれるため、実行時のネットワークアクセスは不要)。
/// コメント・セクション構成の唯一の情報源をこのファイルに一本化し、
/// ウィザードが独自に持つ古い説明文と食い違う事態を防ぐ。
const CONFIG_TEMPLATE: &str = include_str!("../recisdb-proxy.toml.example");

/// `template` の `[section]` セクション内にある `key = "..."` の行を
/// `key = "new_value"` に書き換える。それ以外の行(コメント・他セクション)は
/// 一切変更しない。
///
/// テンプレート側の構造が変わってキーが見つからなかった場合、値が反映されない
/// まま古い既定値が黙って使われる事故を防ぐため panic する(セットアップ時に
/// すぐ気付けるように)。
fn replace_scalar_in_section(template: &str, section: &str, key: &str, new_value: &str) -> String {
    let key_prefix = format!("{key} = ");
    let mut current_section = String::new();
    let mut replaced = false;
    let mut out = String::with_capacity(template.len());

    for line in template.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current_section = name.to_string();
            }
        }

        if !replaced && current_section == section && trimmed.starts_with(&key_prefix) {
            out.push_str(&format!("{key} = \"{new_value}\"\n"));
            replaced = true;
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    assert!(
        replaced,
        "generate_config: `{key}` not found in [{section}] section of recisdb-proxy.toml.example \
         (テンプレートの構造が変わった可能性があります)"
    );

    out
}

/// recisdb-proxy.toml の設定ファイルを生成する。
///
/// `recisdb-proxy.toml.example` をテンプレートとしてそのまま使い、ウィザードで
/// 決まる3つの値(listen/web_listen/データベースパス)だけを書き換える。
/// それ以外の内容([web]/[mirakurun]/[tsreplace]/[preview]/[tls] 等)は
/// テンプレートのままなので、そちらを更新すればウィザードの生成結果にも
/// 自動的に反映される。
pub fn generate_config(listen_addr: &str, web_listen_addr: &str, db_path: &str) -> String {
    let db_path = escape_toml_basic_string(db_path);
    let mut content = CONFIG_TEMPLATE.replace(
        "# このファイルを recisdb-proxy.toml にコピーして編集してください。",
        "# かんたんセットアップにより自動生成されました。",
    );
    content = replace_scalar_in_section(&content, "server", "listen", listen_addr);
    content = replace_scalar_in_section(&content, "server", "web_listen", web_listen_addr);
    content = replace_scalar_in_section(&content, "database", "path", &db_path);
    content
}

// =============================================================================
// クライアント配布用設定一式の出力
// =============================================================================

/// クライアント (TVTest/EDCB を動かすPC) に配布するファイル一式を出力する
/// フォルダ名。インストール先直下に作られる。
pub const CLIENT_CONFIG_DIR: &str = "client-config";

/// bondriver-proxy-client に同梱している INI サンプルをそのまま埋め込み、
/// Address/Tuner だけを実際の値に差し替えて配布用 INI を生成する
/// (generate_config が recisdb-proxy.toml.example を埋め込むのと同じ方式)。
const CLIENT_INI_TEMPLATE: &str =
    include_str!("../../bondriver-proxy-client/BonDriver_NetworkProxy.ini.sample");

/// このマシンのLAN側IPアドレスを推定する。UDPソケットの接続先を
/// 8.8.8.8 に「設定」するだけで実際のパケットは送信されない。
/// (listen が 0.0.0.0 のとき、クライアントに配る INI には具体的な
/// 到達可能アドレスが必要になるため。)
pub fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// 配布用 BonDriver_NetworkProxy.ini を生成する。
pub fn generate_client_ini(server_addr: &str, tuner: &str) -> String {
    let mut replaced_addr = false;
    let mut replaced_tuner = false;
    let content = CLIENT_INI_TEMPLATE
        .lines()
        .map(|line| {
            if !replaced_addr && line.trim_start().starts_with("Address =") {
                replaced_addr = true;
                format!("Address = {server_addr}")
            } else if !replaced_tuner && line.trim_start().starts_with("Tuner =") {
                replaced_tuner = true;
                format!("Tuner = {tuner}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    assert!(
        replaced_addr && replaced_tuner,
        "generate_client_ini: Address/Tuner line not found in BonDriver_NetworkProxy.ini.sample \
         — the sample layout changed; update this generator."
    );
    content + "\r\n"
}

/// クライアント配布用の README を生成する。セットアップウィザードの
/// client-config フォルダ (`includes_channel_files = false`: スキャン前
/// なのでチャンネル設定ファイルは同梱されない) と、ダッシュボードの
/// 「まとめてダウンロード」zip (web/api.rs, `true`: .ch2/ChSet を同梱)
/// の両方で使う。
pub fn generate_client_readme(
    server_addr: &str,
    dashboard_url: &str,
    includes_channel_files: bool,
) -> String {
    let channel_files_step = if includes_channel_files {
        "3. チャンネル設定ファイルの配置\r\n\
         \r\n\
         [TVTest]\r\n\
            同梱の BonDriver_NetworkProxy.ch2 を BonDriver_NetworkProxy.dll と\r\n\
            同じフォルダに置くと、TVTest のチャンネルスキャンを省略できます。\r\n\
            (置かない場合は TVTest の設定 → チャンネルスキャンを一度実行してください)\r\n\
         \r\n\
         [EDCB]\r\n\
            同梱の BonDriver_NetworkProxy(BonDriver_NetworkProxy).ChSet4.txt と\r\n\
            ChSet5.txt を EDCB の Setting フォルダにコピーすると、\r\n\
            EpgDataCap_Bon でのチャンネルスキャンを省略できます。\r\n"
            .to_string()
    } else {
        format!(
            "3. チャンネル設定ファイル (TVTest の .ch2 / EDCB の ChSet4/ChSet5)\r\n\
             \r\n\
             サーバー側でチャンネルスキャンが完了したあと、Webダッシュボード\r\n\
             ({dashboard_url}) の「クライアント設定」タブからダウンロードできます。\r\n\
             .ch2 は BonDriver_NetworkProxy.dll と同じフォルダへ、\r\n\
             ChSet4/ChSet5 は EDCB の Setting フォルダへ置いてください。\r\n\
             (置かない場合は TVTest / EpgDataCap_Bon でチャンネルスキャンを\r\n\
              一度実行すれば同じ結果になります)\r\n"
        )
    };
    format!(
        "recisdb-proxy クライアント設定一式\r\n\
         ==================================\r\n\
         \r\n\
         1. BonDriver_NetworkProxy.dll と BonDriver_NetworkProxy.ini を\r\n\
            TVTest / EDCB の BonDriver フォルダにコピーする\r\n\
         2. TVTest の場合: 設定 → ドライバ で BonDriver_NetworkProxy.dll を選択\r\n\
         {channel_files_step}\
         \r\n\
         接続先サーバー: {server_addr}\r\n\
         (サーバーのIPアドレスが変わった場合は BonDriver_NetworkProxy.ini の\r\n\
          Address 行を書き換えてください)\r\n\
         詳しい手順・チャンネル一覧はダッシュボード ({dashboard_url}) の\r\n\
         「クライアント設定」タブを参照してください。\r\n"
    )
}

/// クライアント配布用フォルダ (install_dir/client-config/) に INI・README を
/// 書き出し、`source_dir` (リリースzipの展開先) に BonDriver_NetworkProxy.dll が
/// あれば同梱する。戻り値は画面ログ用のメッセージ。
pub fn write_client_config_bundle(
    install_dir: &Path,
    source_dir: Option<&Path>,
    server_addr: &str,
    tuner: &str,
    dashboard_url: &str,
) -> Result<Vec<String>, String> {
    let bundle_dir = install_dir.join(CLIENT_CONFIG_DIR);
    std::fs::create_dir_all(&bundle_dir)
        .map_err(|e| format!("{} の作成に失敗しました: {e}", bundle_dir.display()))?;

    let mut log = Vec::new();

    let ini_path = bundle_dir.join("BonDriver_NetworkProxy.ini");
    std::fs::write(&ini_path, generate_client_ini(server_addr, tuner))
        .map_err(|e| format!("INIの書き出しに失敗しました: {e}"))?;

    let readme_path = bundle_dir.join("README.txt");
    std::fs::write(
        &readme_path,
        generate_client_readme(server_addr, dashboard_url, false),
    )
    .map_err(|e| format!("READMEの書き出しに失敗しました: {e}"))?;

    // リリースzipにクライアントDLLが同梱されていればバンドルにコピーする。
    // 無くてもエラーにしない (サーバーだけのパッケージ構成もあり得る)。
    let mut dll_copied = false;
    if let Some(source_dir) = source_dir {
        let dll_src = source_dir.join("BonDriver_NetworkProxy.dll");
        if dll_src.exists() {
            let dll_dest = bundle_dir.join("BonDriver_NetworkProxy.dll");
            std::fs::copy(&dll_src, &dll_dest)
                .map_err(|e| format!("クライアントDLLのコピーに失敗しました: {e}"))?;
            dll_copied = true;
        }
    }

    log.push(format!(
        "クライアント配布用の設定一式を出力しました: {}",
        bundle_dir.display()
    ));
    if !dll_copied {
        log.push(
            "  (BonDriver_NetworkProxy.dll が見つからなかったため INI と README のみ出力しました)"
                .to_string(),
        );
    }
    Ok(log)
}

// =============================================================================
// 本体バイナリのインストール/更新
// =============================================================================

/// [`sync_program_binary`] が実際に行った操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySyncAction {
    /// インストール先に無かったので新規にコピーした。
    FreshInstall,
    /// 既にあったが内容が異なっていた(=古い版だった)ため上書きした。
    Updated,
    /// 既に内容が同一だったため何もしなかった。
    AlreadyUpToDate,
}

/// `source_dir`(このセットアップツール自身が置かれているフォルダ。つまり
/// ダウンロードしたリリースzipの展開先)にある recisdb-proxy 本体を
/// `install_dir` に配置する。バージョン比較はファイル内容そのものの一致で
/// 判定する(実行ファイルのサイズは数MB程度でありハッシュ計算より単純・
/// 確実なため)。設定ファイル・データベース・BonDriverなどのユーザーデータ
/// には一切触れず、実行ファイル本体のみを対象とする。
pub fn sync_program_binary(source_dir: &Path, install_dir: &Path) -> Result<BinarySyncAction, String> {
    let exe_name = if cfg!(windows) {
        "recisdb-proxy.exe"
    } else {
        "recisdb-proxy"
    };
    let source = source_dir.join(exe_name);
    if !source.exists() {
        return Err(format!(
            "{exe_name} が見つかりません。recisdb-proxy-setup と {exe_name} を同じフォルダに配置してください。"
        ));
    }

    std::fs::create_dir_all(install_dir).map_err(|e| e.to_string())?;
    let dest = install_dir.join(exe_name);

    if !dest.exists() {
        std::fs::copy(&source, &dest)
            .map_err(|e| format!("{exe_name} のインストールに失敗しました: {e}"))?;
        return Ok(BinarySyncAction::FreshInstall);
    }

    let identical = match (std::fs::read(&source), std::fs::read(&dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if identical {
        return Ok(BinarySyncAction::AlreadyUpToDate);
    }

    std::fs::copy(&source, &dest).map_err(|e| {
        format!(
            "{exe_name} の更新に失敗しました({e})。{exe_name} が起動中の場合は終了してから、\
             もう一度お試しください。"
        )
    })?;
    Ok(BinarySyncAction::Updated)
}

// =============================================================================
// チューナーのDB登録
// =============================================================================

/// 1件のチューナー登録結果 (画面表示用)
pub struct RegisterResult {
    pub device_path: String,
    pub outcome: Result<i64, String>,
}

/// px4_drv対応チューナーについて、`path`(登録するBonDriver DLLパス)に
/// 対応する `max_instances` を「接続台数 × 1台あたりの同時使用可能数」で
/// 算出する。地上波/衛星でチューナー数が異なる機種(例: PX-Q3U4)でも、
/// DLLファイルごとに正しい値を返す。px4_drv対応機種でない場合や、まだ
/// 台数が分かっていない場合は`None`を返し、呼び出し側は従来の
/// `terrestrial_count + satellite_count` にフォールバックする。
fn px4_max_instances_for_path(tuner: &DetectedTuner, path: &str) -> Option<i32> {
    let pid = tuner.px4_model_pid?;
    let count = tuner.px4_device_count?;
    let file_name = Path::new(path).file_name()?.to_str()?;
    let per_unit = crate::px4_installer::instances_per_unit_for(pid, file_name)?;
    Some(per_unit * count)
}

/// 選択されたチューナーをDBに登録する。
pub fn register_tuners_to_db(
    db: &Database,
    tuners: &[DetectedTuner],
    selected_indices: &[usize],
) -> Vec<RegisterResult> {
    let mut results = Vec::new();

    for &idx in selected_indices {
        let Some(tuner) = tuners.get(idx) else {
            continue;
        };

        for path in &tuner.device_paths {
            let outcome = db.get_or_create_bon_driver(path).inspect(|&id| {
                let total = px4_max_instances_for_path(tuner, path)
                    .unwrap_or(tuner.terrestrial_count + tuner.satellite_count);
                if total > 1 {
                    let _ = db.update_max_instances(id, total);
                }
                if !tuner.group_name.is_empty() {
                    let _ = db.set_group_name(id, Some(&tuner.group_name));
                }
                let _ = db.request_immediate_scan(id);
            });

            results.push(RegisterResult {
                device_path: path.clone(),
                outcome: outcome.map_err(|e| e.to_string()),
            });
        }
    }

    results
}

/// 手動入力された1件のチューナーをDBに登録する。
pub fn register_manual_tuner(
    db: &Database,
    path: &str,
    group_name: &str,
    max_instances: i32,
) -> Result<i64, String> {
    let id = db.get_or_create_bon_driver(path).map_err(|e| e.to_string())?;
    if max_instances > 1 {
        let _ = db.update_max_instances(id, max_instances);
    }
    if !group_name.is_empty() {
        let _ = db.set_group_name(id, Some(group_name));
    }
    let _ = db.request_immediate_scan(id);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "recisdb-proxy-test-{label}-{n}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn sync_program_binary_copies_when_not_installed_yet() {
        let source_dir = unique_temp_dir("sync-src-a");
        let install_dir = unique_temp_dir("sync-dst-a");
        std::fs::create_dir_all(&source_dir).unwrap();

        let exe_name = if cfg!(windows) { "recisdb-proxy.exe" } else { "recisdb-proxy" };
        std::fs::write(source_dir.join(exe_name), b"version-1").unwrap();

        let action = sync_program_binary(&source_dir, &install_dir).unwrap();
        assert_eq!(action, BinarySyncAction::FreshInstall);
        assert_eq!(std::fs::read(install_dir.join(exe_name)).unwrap(), b"version-1");

        std::fs::remove_dir_all(&source_dir).unwrap();
        std::fs::remove_dir_all(&install_dir).unwrap();
    }

    #[test]
    fn sync_program_binary_skips_when_already_up_to_date() {
        let source_dir = unique_temp_dir("sync-src-b");
        let install_dir = unique_temp_dir("sync-dst-b");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&install_dir).unwrap();

        let exe_name = if cfg!(windows) { "recisdb-proxy.exe" } else { "recisdb-proxy" };
        std::fs::write(source_dir.join(exe_name), b"same-content").unwrap();
        std::fs::write(install_dir.join(exe_name), b"same-content").unwrap();

        let action = sync_program_binary(&source_dir, &install_dir).unwrap();
        assert_eq!(action, BinarySyncAction::AlreadyUpToDate);

        std::fs::remove_dir_all(&source_dir).unwrap();
        std::fs::remove_dir_all(&install_dir).unwrap();
    }

    #[test]
    fn sync_program_binary_overwrites_stale_binary() {
        let source_dir = unique_temp_dir("sync-src-c");
        let install_dir = unique_temp_dir("sync-dst-c");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&install_dir).unwrap();

        let exe_name = if cfg!(windows) { "recisdb-proxy.exe" } else { "recisdb-proxy" };
        std::fs::write(source_dir.join(exe_name), b"version-2-new").unwrap();
        std::fs::write(install_dir.join(exe_name), b"version-1-old").unwrap();

        let action = sync_program_binary(&source_dir, &install_dir).unwrap();
        assert_eq!(action, BinarySyncAction::Updated);
        assert_eq!(
            std::fs::read(install_dir.join(exe_name)).unwrap(),
            b"version-2-new"
        );

        std::fs::remove_dir_all(&source_dir).unwrap();
        std::fs::remove_dir_all(&install_dir).unwrap();
    }

    #[test]
    fn sync_program_binary_errors_when_source_missing() {
        let source_dir = unique_temp_dir("sync-src-missing");
        let install_dir = unique_temp_dir("sync-dst-missing");
        // source_dir を作らないまま呼び出す。
        assert!(sync_program_binary(&source_dir, &install_dir).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn find_staged_px4_bondriver_in_finds_previously_installed_files() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir()
            .join(format!("recisdb-proxy-test-staged-px4-{n}-{}", std::process::id()));
        // PX-MLT5PE (0x024e): 単一DLLで検証が簡単なため。
        let model = crate::px4_installer::find_model(0x024e).expect("PX-MLT5PE must be a known model");

        // まだ何もステージングされていない状態では見つからない。
        let none_found = find_staged_px4_bondriver_in(model, &[tmp_root.clone()]);
        assert!(none_found.is_empty());

        // px4_installer::stage_bondriver と同じ配置規則でファイルを置く。
        let staged_dir = tmp_root.join("BonDriver").join(model.bondriver_folder);
        std::fs::create_dir_all(&staged_dir).unwrap();
        for dll_name in model.dll_names {
            std::fs::write(staged_dir.join(dll_name), b"dummy").unwrap();
        }

        let found = find_staged_px4_bondriver_in(model, &[tmp_root.clone()]);
        assert_eq!(found.len(), model.dll_names.len());

        std::fs::remove_dir_all(&tmp_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn find_staged_px4_bondriver_uses_the_given_install_dir_not_cwd() {
        // GUIで選んだインストール先 (CWDとは無関係な場所) に配置済みの
        // BonDriverが、カレントディレクトリ相対の既定パスしか見ない実装に
        // 戻っていないかを確認するリグレッションテスト。
        let install_dir = std::env::temp_dir().join(format!(
            "recisdb-proxy-test-custom-install-dir-{}",
            std::process::id()
        ));
        let model = crate::px4_installer::find_model(0x0052).expect("DTV03A-1TU must be a known model");

        let staged_dir = install_dir.join("BonDriver").join(model.bondriver_folder);
        std::fs::create_dir_all(&staged_dir).unwrap();
        for dll_name in model.dll_names {
            std::fs::write(staged_dir.join(dll_name), b"dummy").unwrap();
        }

        let found = find_staged_px4_bondriver(model, &install_dir);
        assert_eq!(found.len(), model.dll_names.len());
        assert!(found[0].contains(install_dir.to_string_lossy().as_ref()));

        std::fs::remove_dir_all(&install_dir).unwrap();
    }

    #[test]
    fn escape_toml_basic_string_doubles_backslashes() {
        assert_eq!(
            escape_toml_basic_string(r"C:\DTV\recisdb-proxy-rs\recisdb-proxy.db"),
            r"C:\\DTV\\recisdb-proxy-rs\\recisdb-proxy.db"
        );
        assert_eq!(escape_toml_basic_string("no_backslashes.db"), "no_backslashes.db");
    }

    #[test]
    fn generate_client_ini_replaces_address_and_tuner() {
        let ini = generate_client_ini("192.168.1.10:40070", "PX-MLT");
        assert!(ini.contains("Address = 192.168.1.10:40070"));
        assert!(ini.contains("Tuner = PX-MLT"));
        // 元サンプルの他の設定 (デフォルト値・説明コメント) は保持される
        assert!(ini.contains("ServiceFilter = all"));
        assert!(ini.contains("[Logging]"));
        // 差し替え前の値が残っていないこと
        assert!(!ini.contains("Address = 127.0.0.1:40070"));
    }

    #[test]
    fn write_client_config_bundle_outputs_ini_readme_and_optional_dll() {
        let base = std::env::temp_dir().join(format!("client_bundle_test_{}", std::process::id()));
        let install_dir = base.join("install");
        let source_dir = base.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("BonDriver_NetworkProxy.dll"), b"dummy dll").unwrap();

        let log = write_client_config_bundle(
            &install_dir,
            Some(&source_dir),
            "192.168.1.10:40070",
            "PX-MLT",
            "http://192.168.1.10:40080",
        )
        .unwrap();
        assert!(!log.is_empty());

        let bundle = install_dir.join(CLIENT_CONFIG_DIR);
        let ini = std::fs::read_to_string(bundle.join("BonDriver_NetworkProxy.ini")).unwrap();
        assert!(ini.contains("Address = 192.168.1.10:40070"));
        assert!(ini.contains("Tuner = PX-MLT"));
        let readme = std::fs::read_to_string(bundle.join("README.txt")).unwrap();
        assert!(readme.contains("http://192.168.1.10:40080"));
        assert!(bundle.join("BonDriver_NetworkProxy.dll").exists());

        // DLLが無いソースでも失敗せず INI/README は出力される
        let install_dir2 = base.join("install2");
        let log2 = write_client_config_bundle(
            &install_dir2,
            None,
            "10.0.0.2:40070",
            "",
            "http://10.0.0.2:40080",
        )
        .unwrap();
        assert!(log2.iter().any(|l| l.contains("INI と README のみ")));
        assert!(install_dir2.join(CLIENT_CONFIG_DIR).join("BonDriver_NetworkProxy.ini").exists());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn generate_config_embeds_all_fields() {
        let toml = generate_config("0.0.0.0:40070", "0.0.0.0:40080", "recisdb-proxy.db");
        assert!(toml.contains(r#"listen = "0.0.0.0:40070""#));
        assert!(toml.contains(r#"web_listen = "0.0.0.0:40080""#));
        assert!(toml.contains(r#"path = "recisdb-proxy.db""#));
    }

    #[test]
    fn generate_config_is_valid_toml_and_up_to_date_with_sections() {
        let generated = generate_config("0.0.0.0:40070", "0.0.0.0:40080", "recisdb-proxy.db");

        // recisdb-proxy.toml.example に存在する全セクションを、コメントアウト
        // 済みでもよいので案内として含んでいることを確認する
        // (main.rs の ConfigFile が持つセクション: server/database/logging/
        // web/mirakurun/tsreplace/preview/tls)。古いテンプレートへの
        // 先祖返りを防ぐリグレッションテスト。
        for section in ["[web]", "[mirakurun]", "[tsreplace]", "[preview]", "[tls]"] {
            assert!(
                generated.contains(section),
                "generated config is missing a mention of {section}"
            );
        }

        // 構文として壊れていないことも確認する(コメントアウトされた
        // {{SID}} 等のエスケープミスで壊れやすいため)。
        toml::from_str::<toml::Value>(&generated).expect("generated config must be valid TOML");
    }

    #[test]
    fn generate_config_is_derived_from_the_actual_example_file() {
        // recisdb-proxy.toml.example を直接埋め込んでおり、独自にテキストを
        // 複製していないことを確認する(このテストは
        // recisdb-proxy.toml.example と setup_helpers.rs の内容が食い違う
        // 状況を根本的に防ぐためのもの)。動的に書き換える3値
        // (listen/web_listen/database.path) 以外の行は完全に一致するはず。
        let generated = generate_config("1.2.3.4:1", "5.6.7.8:2", "custom.db");
        let template_lines: Vec<&str> = CONFIG_TEMPLATE.lines().collect();
        let generated_lines: Vec<&str> = generated.lines().collect();
        assert_eq!(
            template_lines.len(),
            generated_lines.len(),
            "generated config must have the same number of lines as the template"
        );

        let mut differing_lines = 0;
        for (t, g) in template_lines.iter().zip(generated_lines.iter()) {
            if t != g {
                differing_lines += 1;
            }
        }
        // 冒頭の案内コメント1行 + listen / web_listen / database.path の3行、
        // 合計4行だけが変わっているはず。
        assert_eq!(differing_lines, 4, "only the wizard-controlled lines should differ from the template");
    }

    #[test]
    #[should_panic(expected = "not found in [server] section")]
    fn replace_scalar_in_section_panics_when_key_missing() {
        replace_scalar_in_section("[server]\nfoo = \"bar\"\n", "server", "listen", "x");
    }

    #[test]
    fn generate_config_escapes_windows_style_backslash_paths() {
        // インストール先フォルダの既定値 (C:\DTV\recisdb-proxy-rs) のような
        // バックスラッシュ区切りのパスがdb_pathに渡ると、TOMLの文字列内で
        // バックスラッシュがエスケープ文字として解釈されてしまい
        // (`\D`は不正なエスケープ、`\r`は復帰文字になってしまう等)、
        // recisdb-proxy本体がこの設定ファイルを読み込めず起動に失敗する
        // 不具合があった。
        let db_path = r"C:\DTV\recisdb-proxy-rs\recisdb-proxy.db";
        let generated = generate_config("0.0.0.0:40070", "0.0.0.0:40080", db_path);

        let parsed: toml::Value = toml::from_str(&generated)
            .expect("generated config with a Windows-style path must still be valid TOML");

        let roundtripped_path = parsed
            .get("database")
            .and_then(|d| d.get("path"))
            .and_then(|p| p.as_str())
            .expect("database.path must be a string");
        assert_eq!(roundtripped_path, db_path, "path must round-trip exactly, not be mangled by escape sequences");
    }

    #[test]
    fn register_tuners_to_db_reports_one_result_per_device_path() {
        let db = Database::open_in_memory().unwrap();
        let tuners = vec![DetectedTuner {
            name: "test".to_string(),
            device_paths: vec!["dev-a".to_string(), "dev-b".to_string()],
            group_name: "TESTGRP".to_string(),
            terrestrial_count: 2,
            satellite_count: 0,
            bondriver_url: String::new(),
            px4_model_pid: None,
            px4_device_count: None,
        }];

        let results = register_tuners_to_db(&db, &tuners, &[0]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome.is_ok()));
    }

    #[test]
    fn register_tuners_to_db_scales_px4_max_instances_by_device_count() {
        let db = Database::open_in_memory().unwrap();
        // terrestrial_count + satellite_count が 0 のままでも、
        // px4_device_count(実際にUSB列挙で数えた接続台数)× 1台あたりの
        // 同時使用可能数(PX-W3U4は地デジ2ch)が max_instances に反映される。
        let tuners = vec![DetectedTuner {
            name: "PLEX PX-W3U4 x2".to_string(),
            device_paths: vec!["BonDriver_PX4-T.dll".to_string()],
            group_name: "BonDriver_PX4".to_string(),
            terrestrial_count: 0,
            satellite_count: 0,
            bondriver_url: String::new(),
            px4_model_pid: Some(0x083f),
            px4_device_count: Some(2),
        }];

        let results = register_tuners_to_db(&db, &tuners, &[0]);
        assert_eq!(results.len(), 1);
        assert!(results[0].outcome.is_ok());

        let max_instances = db.get_max_instances_for_path("BonDriver_PX4-T.dll").unwrap();
        assert_eq!(max_instances, 4); // 2台 x 2ch/台
    }

    #[test]
    fn register_tuners_to_db_uses_per_band_capacity_for_q3u4() {
        let db = Database::open_in_memory().unwrap();
        // PX-Q3U4は地デジ・BS/CSともに1台あたり4chなので、S/Tどちらの
        // DLLパスでも同じ倍率(台数x4)が反映される。
        let tuners = vec![DetectedTuner {
            name: "PLEX PX-Q3U4 x1".to_string(),
            device_paths: vec![
                "BonDriver_PX4-S.dll".to_string(),
                "BonDriver_PX4-T.dll".to_string(),
            ],
            group_name: "BonDriver_PX4".to_string(),
            terrestrial_count: 0,
            satellite_count: 0,
            bondriver_url: String::new(),
            px4_model_pid: Some(0x084a),
            px4_device_count: Some(1),
        }];

        register_tuners_to_db(&db, &tuners, &[0]);

        assert_eq!(db.get_max_instances_for_path("BonDriver_PX4-S.dll").unwrap(), 4);
        assert_eq!(db.get_max_instances_for_path("BonDriver_PX4-T.dll").unwrap(), 4);
    }

    #[test]
    fn register_manual_tuner_sets_group_and_instances() {
        let db = Database::open_in_memory().unwrap();
        let id = register_manual_tuner(&db, "manual-path", "MANUAL", 3).unwrap();
        assert!(id > 0);
    }
}
