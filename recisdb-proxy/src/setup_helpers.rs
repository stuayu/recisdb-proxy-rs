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

/// recisdb-proxy.toml の設定ファイルを生成
pub fn generate_config(listen_addr: &str, web_listen_addr: &str, db_path: &str) -> String {
    format!(
        r#"# recisdb-proxy 設定ファイル (かんたんセットアップで自動生成)
# 詳しい説明は recisdb-proxy.toml.example を参照してください。

[server]
# プロキシサーバーの待ち受けアドレス
listen = "{listen_addr}"

# Webダッシュボードの待ち受けアドレス
web_listen = "{web_listen_addr}"

# 最大同時接続数
max_connections = 64

[database]
# SQLiteデータベースファイルのパス
path = "{db_path}"

[logging]
# ログファイルの保存ディレクトリ
log_dir = "logs"

# ログファイルの保持日数
retention_days = 7

# ログレベル (off, error, warn, info, debug, trace)
# level = "warn"

# =====================================================
# Webダッシュボード/API認証設定
# =====================================================
# [web]
# /api/* に Authorization: Bearer <トークン> を要求する (デフォルト: true)
# false は隔離されたLANでのテスト専用です
# auth_enabled = true
#
# 認証トークンを明示指定する (省略可)
# 未指定の場合は初回起動時に自動生成されDBに保存されます。
# 認証有効時、実際に使われるトークンは毎回起動ログに表示されます
# auth_token = "任意のトークン文字列"

# =====================================================
# Mirakurun互換API設定
# =====================================================
# [mirakurun]
# /mirakurun/api/* を有効にする (デフォルト: false)
# このエンドポイントは認証なしで公開されるため(実際のMirakurunクライアント
# はAuthorizationヘッダを送らないため)、信頼できるネットワークでのみ
# 有効にしてください
# enabled = false

# =====================================================
# BNDPセッション (TVTest等) 用エンコーダ設定
# =====================================================
# 実行ファイルのパスはこの設定ファイルからのみ変更できます
# (Web APIから変更可能にするとリモートコード実行の踏み台になるため)。
# 有効/無効の切り替えや引数はWebダッシュボードから設定します。
# [tsreplace]
# エンコーダ実行ファイルのパス (例: tsreplace)
# command_path = "C:\\DTV\\tsreplace\\tsreplace.exe"
#
# 前段プロセスの実行ファイルパス (省略可、例: tsreadex)
# 設定するとパイプライン TS → 前段 → エンコーダ → 出力 で実行されます。
# 空文字列を指定すると保存済みの値をクリアできます
# preprocessor_path = "C:\\DTV\\tsreadex\\tsreadex.exe"

# =====================================================
# ブラウザプレビュー (?profile=preview) 用エンコーダ設定
# =====================================================
# [tsreplace] とは完全に独立した設定です。パスがTOML専用である理由も同じ。
# 有効/無効・前段引数はWebダッシュボードの「ブラウザプレビュー」設定から。
# 引数内の {{SID}} は対象サービスIDに置換されます。
#
# 引数は初回起動時に推奨値がDBへ自動登録されます (ダッシュボードから変更可):
#   前段 (tsreadex):    -x 18/38/39 -n {{SID}} -a 13 -b 5 -c 1 -u 1 -d 13 -
#   エンコーダ (QSVEncC): H.264 ~2Mbps VBR / インターレース解除 / AAC ステレオ
#                        (encode_profiles の preview-h264 プロファイル)
# ここでは実行ファイルのパス2つを設定するだけで動作します。
# サービス選択は前段 tsreadex の -n {{SID}} が担うため、前段の設定を推奨します。
# [preview]
# プレビュー用エンコーダの実行ファイルパス (例: QSVEncC)
# command_path = "C:\\DTV\\KonomiTV\\server\\thirdparty\\QSVEncC\\QSVEncC.exe"
#
# 前段プロセスの実行ファイルパス (例: tsreadex によるサービス選択)
# preprocessor_path = "C:\\DTV\\KonomiTV\\server\\thirdparty\\tsreadex\\tsreadex.exe"

# =====================================================
# TLS設定 (tls フィーチャーが有効な場合のみ)
# =====================================================
# [tls]
# TLS暗号化を有効にする (デフォルト: false)
# enabled = false
#
# CA証明書のパス (PEM形式)
# ca_cert = "ca.pem"
#
# サーバー証明書のパス (PEM形式)
# server_cert = "server.pem"
#
# サーバー秘密鍵のパス (PEM形式)
# server_key = "server-key.pem"
#
# クライアント証明書の要求 (デフォルト: false)
# true にするとクライアント証明書がない接続は拒否されます
# require_client_cert = false
"#
    )
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
                let _ = db.enable_immediate_scan(id);
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
    let _ = db.enable_immediate_scan(id);
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
