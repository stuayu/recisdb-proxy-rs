//! recisdb-proxy 初回セットアップツール
//!
//! DBが存在しない場合のQuick Startを支援するための対話式セットアップツール。
//! PCに接続されているチューナーを自動検出し、BonDriverの設定を行います。

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use recisdb_proxy::database::Database;

// =============================================================================
// チューナー定義
// =============================================================================

/// 既知のチューナーデバイス情報
#[allow(dead_code)]
struct KnownTuner {
    /// デバイス名 (人間向けの表示名)
    name: &'static str,
    /// USB Vendor ID
    usb_vendor_id: u16,
    /// USB Product ID
    usb_product_id: u16,
    /// グループ名 (同系統チューナーの統合用)
    group_name: &'static str,
    /// 地上波対応数
    terrestrial_count: i32,
    /// BS/CS (衛星) 対応数
    satellite_count: i32,
    /// BonDriverのダウンロードURL (後から設定)
    bondriver_url: &'static str,
    /// BonDriverのDLLファイル名パターン (Windows)
    bondriver_dll_pattern: &'static str,
    /// Linuxデバイスパスのパターン
    linux_device_pattern: &'static str,
}

/// 既知のチューナーデバイス一覧
/// NOTE: bondriver_url は後から正式なものを設定してください
#[allow(dead_code)]
const KNOWN_TUNERS: &[KnownTuner] = &[
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
#[allow(dead_code)]
struct DetectedTuner {
    /// チューナー名
    name: String,
    /// デバイスパスのリスト
    device_paths: Vec<String>,
    /// グループ名
    group_name: String,
    /// 地上波チューナー数
    terrestrial_count: i32,
    /// 衛星チューナー数
    satellite_count: i32,
    /// BonDriverのDLLパターン
    bondriver_dll_pattern: String,
    /// BonDriverのダウンロードURL
    bondriver_url: String,
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
                                    adapters.push(format!(
                                        "/dev/dvb/{}/{}",
                                        name, sub_name
                                    ));
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
                bondriver_dll_pattern: String::new(),
                bondriver_url: String::new(),
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
                    bondriver_dll_pattern: known
                        .map_or(String::new(), |k| k.bondriver_dll_pattern.to_string()),
                    bondriver_url: known
                        .map_or(String::new(), |k| k.bondriver_url.to_string()),
                });
            }
        }
    }

    detected
}

/// Windowsでのチューナーデバイス検出 (BonDriver DLLの検索)
#[cfg(target_os = "windows")]
fn detect_tuners_windows() -> Vec<DetectedTuner> {
    let mut detected = Vec::new();

    // BonDriver DLLの検索パス候補
    let search_dirs = [
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
                        bondriver_dll_pattern: known
                            .map_or(String::new(), |k| k.bondriver_dll_pattern.to_string()),
                        bondriver_url: known
                            .map_or(String::new(), |k| k.bondriver_url.to_string()),
                    });
                }
            }
        }
    }

    detected
}

/// チューナーを検出
fn detect_tuners() -> Vec<DetectedTuner> {
    #[cfg(target_os = "linux")]
    {
        detect_tuners_linux()
    }
    #[cfg(target_os = "windows")]
    {
        detect_tuners_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

// =============================================================================
// 設定ファイル生成
// =============================================================================

/// recisdb-proxy.toml の設定ファイルを生成
fn generate_config(
    listen_addr: &str,
    web_listen_addr: &str,
    db_path: &str,
) -> String {
    format!(
        r#"# recisdb-proxy 設定ファイル (自動生成)

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
"#
    )
}

// =============================================================================
// ユーザー入力ヘルパー
// =============================================================================

/// ユーザーから文字列入力を受け取る (デフォルト値付き)
fn prompt(message: &str, default: &str) -> String {
    if default.is_empty() {
        print!("{}: ", message);
    } else {
        print!("{} [{}]: ", message, default);
    }
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let input = input.trim().to_string();

    if input.is_empty() {
        default.to_string()
    } else {
        input
    }
}

/// はい/いいえの確認
fn confirm(message: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{} {}: ", message, suffix);
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let input = input.trim().to_lowercase();

    if input.is_empty() {
        default_yes
    } else {
        input == "y" || input == "yes" || input == "はい"
    }
}

/// 番号選択
#[allow(dead_code)]
fn select_number(message: &str, min: usize, max: usize) -> usize {
    loop {
        print!("{} ({}-{}): ", message, min, max);
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        if let Ok(n) = input.trim().parse::<usize>() {
            if n >= min && n <= max {
                return n;
            }
        }
        println!("  {} から {} の間の数値を入力してください。", min, max);
    }
}

// =============================================================================
// セットアップ処理
// =============================================================================

/// 検出されたチューナーを表示
fn display_detected_tuners(tuners: &[DetectedTuner]) {
    println!("\n検出されたチューナーデバイス:");
    println!("{}", "=".repeat(60));

    if tuners.is_empty() {
        println!("  チューナーデバイスは検出されませんでした。");
        println!("  手動でチューナーパスを入力してください。");
        return;
    }

    for (i, tuner) in tuners.iter().enumerate() {
        println!(
            "  [{}] {} (グループ: {})",
            i + 1,
            tuner.name,
            tuner.group_name
        );
        if tuner.terrestrial_count > 0 || tuner.satellite_count > 0 {
            println!(
                "      地上波: {}ch / 衛星(BS/CS): {}ch",
                tuner.terrestrial_count, tuner.satellite_count
            );
        }
        for path in &tuner.device_paths {
            println!("      デバイス: {}", path);
        }
    }
    println!("{}", "=".repeat(60));
}

/// チューナーをDBに登録
fn register_tuners_to_db(db: &Database, tuners: &[DetectedTuner], selected: &[usize]) {
    for &idx in selected {
        let tuner = &tuners[idx];

        for path in &tuner.device_paths {
            match db.get_or_create_bon_driver(path) {
                Ok(id) => {
                    println!("  登録完了: {} (ID: {})", path, id);

                    // max_instances の設定
                    let total = tuner.terrestrial_count + tuner.satellite_count;
                    if total > 1 {
                        if let Err(e) = db.update_max_instances(id, total) {
                            eprintln!("  警告: max_instances の設定に失敗: {}", e);
                        }
                    }

                    // グループ名の設定
                    if !tuner.group_name.is_empty() {
                        if let Err(e) = db.set_group_name(id, Some(&tuner.group_name)) {
                            eprintln!("  警告: グループ名の設定に失敗: {}", e);
                        }
                    }

                    // 自動スキャンを有効化して即時スキャンをスケジュール
                    if let Err(e) = db.enable_immediate_scan(id) {
                        eprintln!("  警告: 自動スキャンの設定に失敗: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("  エラー: {} の登録に失敗: {}", path, e);
                }
            }
        }
    }
}

/// Windowsでの手動チューナーパス入力
fn prompt_manual_tuner_paths() -> Vec<(String, String, i32)> {
    let mut paths = Vec::new();

    println!("\nチューナーのパスを手動で入力してください。");
    println!("空行を入力すると終了します。\n");

    loop {
        let path = prompt("チューナーパス (DLLパスまたはデバイスパス)", "");
        if path.is_empty() {
            break;
        }

        let group = prompt("グループ名 (同種チューナーの統合用, 省略可)", "");
        let max_str = prompt("最大同時使用チャンネル数", "1");
        let max_instances = max_str.parse::<i32>().unwrap_or(1);

        paths.push((path, group, max_instances));

        if !confirm("さらにチューナーを追加しますか?", false) {
            break;
        }
    }

    paths
}

/// メインのセットアップフロー
fn run_setup() -> Result<(), Box<dyn std::error::Error>> {
    println!("=====================================================");
    println!("  recisdb-proxy 初回セットアップ");
    println!("=====================================================");
    println!();
    println!("このツールは以下の初期設定を行います:");
    println!("  1. 設定ファイル (recisdb-proxy.toml) の生成");
    println!("  2. データベースの初期化");
    println!("  3. チューナーデバイスの自動検出と登録");
    println!("  4. BonDriverのダウンロード準備");
    println!();

    // ----- ステップ 1: 基本設定 -----
    println!("--- ステップ 1/4: 基本設定 ---\n");

    let listen_addr = prompt("プロキシサーバーの待ち受けアドレス", "0.0.0.0:12345");
    let web_listen_addr = prompt("Webダッシュボードの待ち受けアドレス", "0.0.0.0:40080");
    let db_path = prompt("データベースファイルのパス", "recisdb-proxy.db");
    let config_path = prompt("設定ファイルの保存先", "recisdb-proxy.toml");

    // ----- ステップ 2: 設定ファイル生成 -----
    println!("\n--- ステップ 2/4: 設定ファイル生成 ---\n");

    let config_file_path = Path::new(&config_path);
    if config_file_path.exists() {
        if !confirm(
            &format!("{} は既に存在します。上書きしますか?", config_path),
            false,
        ) {
            println!("  設定ファイルの上書きをスキップしました。");
        } else {
            let config_content = generate_config(&listen_addr, &web_listen_addr, &db_path);
            std::fs::write(config_file_path, config_content)?;
            println!("  設定ファイルを生成しました: {}", config_path);
        }
    } else {
        let config_content = generate_config(&listen_addr, &web_listen_addr, &db_path);
        std::fs::write(config_file_path, config_content)?;
        println!("  設定ファイルを生成しました: {}", config_path);
    }

    // ----- ステップ 3: データベース初期化とチューナー検出 -----
    println!("\n--- ステップ 3/4: データベース初期化とチューナー検出 ---\n");

    let db_file_path = Path::new(&db_path);
    let db_exists = db_file_path.exists();

    if db_exists {
        println!("  データベースが既に存在します: {}", db_path);
        if !confirm("既存のデータベースを使用しますか? (いいえの場合、新規作成)", true) {
            // バックアップを作成してから再作成
            let backup_path = format!("{}.backup", db_path);
            std::fs::rename(db_file_path, &backup_path)?;
            println!("  バックアップを作成しました: {}", backup_path);
        }
    }

    let db = Database::open(&db_path)?;
    println!("  データベースを初期化しました: {}", db_path);

    // チューナー検出
    println!("\nチューナーデバイスを検出中...");
    let detected = detect_tuners();
    display_detected_tuners(&detected);

    if !detected.is_empty() {
        println!("\n登録するチューナーを選択してください。");
        println!("  a: すべて登録");
        println!("  番号: 個別に選択 (カンマ区切りで複数指定可)");
        println!("  n: スキップ (手動入力に進む)");

        print!("\n選択: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        if input == "a" || input == "all" || input == "すべて" {
            let all_indices: Vec<usize> = (0..detected.len()).collect();
            register_tuners_to_db(&db, &detected, &all_indices);
        } else if input != "n" && input != "no" && input != "スキップ" {
            let indices: Vec<usize> = input
                .split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n >= 1 && n <= detected.len())
                .map(|n| n - 1)
                .collect();
            if !indices.is_empty() {
                register_tuners_to_db(&db, &detected, &indices);
            }
        }
    }

    // 手動入力
    if confirm("\n手動でチューナーパスを追加しますか?", detected.is_empty()) {
        let manual_paths = prompt_manual_tuner_paths();
        for (path, group, max_instances) in &manual_paths {
            match db.get_or_create_bon_driver(path) {
                Ok(id) => {
                    println!("  登録完了: {} (ID: {})", path, id);
                    if *max_instances > 1 {
                        let _ = db.update_max_instances(id, *max_instances);
                    }
                    if !group.is_empty() {
                        let _ = db.set_group_name(id, Some(group));
                    }
                    let _ = db.enable_immediate_scan(id);
                }
                Err(e) => {
                    eprintln!("  エラー: {} の登録に失敗: {}", path, e);
                }
            }
        }
    }

    // ----- ステップ 4: BonDriverダウンロード -----
    println!("\n--- ステップ 4/4: BonDriverダウンロード ---\n");

    let has_download_urls = detected.iter().any(|t| !t.bondriver_url.is_empty());

    if has_download_urls {
        if confirm("検出されたチューナーのBonDriverをダウンロードしますか?", true) {
            for tuner in &detected {
                if tuner.bondriver_url.is_empty() {
                    continue;
                }
                println!(
                    "  {} 用BonDriver: {}",
                    tuner.name, tuner.bondriver_url
                );
                // TODO: 実際のダウンロード処理を実装
                // download_bondriver(&tuner.bondriver_url, &bondriver_dir)?;
            }
        }
    } else {
        println!("  BonDriverのダウンロードURLが未設定です。");
        println!("  BonDriverは手動でダウンロードし、適切なディレクトリに配置してください。");
        println!();
        println!("  参考URL:");
        println!("    - PLEX社チューナー用: (後から設定されます)");
        println!("    - PT3用: (後から設定されます)");
        println!();
        println!("  ダウンロード後、Webダッシュボード (http://localhost:40080) から");
        println!("  チューナーパスを更新できます。");
    }

    // ----- 完了 -----
    println!("\n=====================================================");
    println!("  セットアップ完了!");
    println!("=====================================================");
    println!();
    println!("以下のコマンドで recisdb-proxy を起動できます:");
    println!();
    println!("  recisdb-proxy --config {}", config_path);
    println!();
    println!("Webダッシュボード: http://{}", web_listen_addr);
    println!();
    println!("起動後、以下の操作が可能です:");
    println!("  - チューナーの max_instances (最大同時使用数) の調整");
    println!("  - チャンネルスキャンの実行");
    println!("  - アラートルールの設定");
    println!();

    Ok(())
}

fn main() {
    if let Err(e) = run_setup() {
        eprintln!("\nセットアップ中にエラーが発生しました: {}", e);
        std::process::exit(1);
    }
}
