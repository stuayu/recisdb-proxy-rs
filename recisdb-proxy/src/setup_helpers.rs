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
                        bondriver_url: known
                            .map_or(String::new(), |k| k.bondriver_url.to_string()),
                    });
                }
            }
        }
    }

    detected
}

/// チューナーを検出する。時間がかかることがあるため、GUIから呼ぶ場合はワーカー
/// スレッド上で実行すること。
pub fn detect_tuners() -> Vec<DetectedTuner> {
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
pub fn generate_config(listen_addr: &str, web_listen_addr: &str, db_path: &str) -> String {
    format!(
        r#"# recisdb-proxy 設定ファイル (かんたんセットアップで自動生成)

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
// チューナーのDB登録
// =============================================================================

/// 1件のチューナー登録結果 (画面表示用)
pub struct RegisterResult {
    pub device_path: String,
    pub outcome: Result<i64, String>,
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
                let total = tuner.terrestrial_count + tuner.satellite_count;
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

    #[test]
    fn generate_config_embeds_all_fields() {
        let toml = generate_config("0.0.0.0:40070", "0.0.0.0:40080", "recisdb-proxy.db");
        assert!(toml.contains(r#"listen = "0.0.0.0:40070""#));
        assert!(toml.contains(r#"web_listen = "0.0.0.0:40080""#));
        assert!(toml.contains(r#"path = "recisdb-proxy.db""#));
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
        }];

        let results = register_tuners_to_db(&db, &tuners, &[0]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome.is_ok()));
    }

    #[test]
    fn register_manual_tuner_sets_group_and_instances() {
        let db = Database::open_in_memory().unwrap();
        let id = register_manual_tuner(&db, "manual-path", "MANUAL", 3).unwrap();
        assert!(id > 0);
    }
}
