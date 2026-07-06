//! px4_drv for WinUSB の自動ダウンロード・インストール
//!
//! [tsukumijima/DTV-Builds](https://github.com/tsukumijima/DTV-Builds) で配布されている
//! [tsukumijima/px4_drv](https://github.com/tsukumijima/px4_drv) の WinUSB 版ビルド済みアーカイブを
//! 取得し、検出済みの PLEX/e-Better 製チューナーに対応する BonDriver とドライバをセットアップする。
//!
//! 対応デバイスの USB VID/PID とドライバファイル名の対応関係は、px4_drv 本体の
//! `driver/px4_usb.h`(PID定義) と配布アーカイブ内の `Driver/*.inf`(ハードウェアID)を
//! 突き合わせて確認したもの(2026年7月時点)。

use std::path::{Path, PathBuf};

/// PLEX / e-Better 製チューナーの USB Vendor ID (px4_drv 対応機種はすべて共通)。
pub const PX4_USB_VENDOR_ID: u16 = 0x0511;

/// px4_drv for WinUSB が対応するチューナー1機種分の情報。
pub struct Px4Model {
    /// 表示用のデバイス名
    pub label: &'static str,
    /// USB Product ID (Vendor ID は [`PX4_USB_VENDOR_ID`] で固定)
    pub usb_pid: u16,
    /// 配布アーカイブ `Driver/` 以下の .inf ファイル名
    pub inf_name: &'static str,
    /// 配布アーカイブ内の BonDriver フォルダ名 (`{bondriver_folder}_32bit` / `_64bit`)
    pub bondriver_folder: &'static str,
    /// フォルダ内に含まれる BonDriver DLL のファイル名 (地上波/衛星で分かれる機種は2つ)
    pub dll_names: &'static [&'static str],
}

/// px4_drv for WinUSB が対応する既知のチューナー一覧。
pub const PX4_MODELS: &[Px4Model] = &[
    Px4Model {
        label: "PLEX PX-W3U4",
        usb_pid: 0x083f,
        inf_name: "PX-W3U4.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-Q3U4",
        usb_pid: 0x084a,
        inf_name: "PX-Q3U4.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-W3PE4",
        usb_pid: 0x023f,
        inf_name: "PX-W3PE4.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-Q3PE4",
        usb_pid: 0x024a,
        inf_name: "PX-Q3PE4.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-W3PE5",
        usb_pid: 0x073f,
        inf_name: "PX-W3PE5.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-Q3PE5",
        usb_pid: 0x074a,
        inf_name: "PX-Q3PE5.inf",
        bondriver_folder: "BonDriver_PX4",
        dll_names: &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"],
    },
    Px4Model {
        label: "PLEX PX-MLT5PE",
        usb_pid: 0x024e,
        inf_name: "PX-MLT5PE.inf",
        bondriver_folder: "BonDriver_PX-MLT",
        dll_names: &["BonDriver_PX-MLT.dll"],
    },
    Px4Model {
        label: "PLEX PX-MLT8PE (Rev.3)",
        usb_pid: 0x0252,
        inf_name: "PX-MLT8PE3.inf",
        bondriver_folder: "BonDriver_PX-MLT",
        dll_names: &["BonDriver_PX-MLT.dll"],
    },
    Px4Model {
        label: "PLEX PX-MLT8PE (Rev.5)",
        usb_pid: 0x0253,
        inf_name: "PX-MLT8PE5.inf",
        bondriver_folder: "BonDriver_PX-MLT",
        dll_names: &["BonDriver_PX-MLT.dll"],
    },
    Px4Model {
        label: "e-Better DTV02A-4TS-P",
        usb_pid: 0x0254,
        inf_name: "DTV02A-4TS-P.inf",
        bondriver_folder: "BonDriver_PX-MLT",
        dll_names: &["BonDriver_PX-MLT.dll"],
    },
    Px4Model {
        label: "e-Better DTV02A-1T1S-U",
        usb_pid: 0x004b,
        inf_name: "DTV02A-1T1S-U_ISDB2056.inf",
        bondriver_folder: "BonDriver_ISDB2056",
        dll_names: &["BonDriver_ISDB2056.dll"],
    },
    Px4Model {
        label: "e-Better DTV02A-1T1S-U (ロット番号2309以降)",
        usb_pid: 0x084b,
        inf_name: "DTV02A-1T1S-U_ISDB2056N.inf",
        bondriver_folder: "BonDriver_ISDB2056N",
        dll_names: &["BonDriver_ISDB2056N.dll"],
    },
    Px4Model {
        label: "e-Better DTV03A-1TU",
        usb_pid: 0x0052,
        inf_name: "DTV03A-1TU_ISDBT2071.inf",
        bondriver_folder: "BonDriver_ISDBT2071",
        dll_names: &["BonDriver_ISDBT2071.dll"],
    },
    Px4Model {
        label: "PLEX PX-M1UR",
        usb_pid: 0x0854,
        inf_name: "PX-M1UR.inf",
        bondriver_folder: "BonDriver_PX-M1UR",
        dll_names: &["BonDriver_PX-M1UR.dll"],
    },
    Px4Model {
        label: "PLEX PX-S1UR",
        usb_pid: 0x0855,
        inf_name: "PX-S1UR.inf",
        bondriver_folder: "BonDriver_PX-S1UR",
        dll_names: &["BonDriver_PX-S1UR.dll"],
    },
];

/// USB PID から対応する [`Px4Model`] を引く。
pub fn find_model(usb_pid: u16) -> Option<&'static Px4Model> {
    PX4_MODELS.iter().find(|m| m.usb_pid == usb_pid)
}

/// 現在接続されている px4_drv 対応チューナーを検出する (Windows専用)。
///
/// `Get-PnpDevice` (PnP デバイス列挙) の InstanceId から `VID_0511&PID_xxxx` を
/// 探すことで、ドライバがまだ入っていない未認識のデバイスも検出できる。
/// (BonDriver DLL の有無で判定する既存の検出方式と違い、こちらは
/// 「初めて挿した直後で何も入っていない」状態でも見つけられる。)
#[cfg(target_os = "windows")]
pub fn detect_connected_px4_devices() -> Vec<&'static Px4Model> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-PnpDevice -PresentOnly | Select-Object -ExpandProperty InstanceId",
        ])
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout).to_uppercase();

    PX4_MODELS
        .iter()
        .filter(|model| {
            let needle = format!("VID_{:04X}&PID_{:04X}", PX4_USB_VENDOR_ID, model.usb_pid);
            text.contains(&needle)
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
pub fn detect_connected_px4_devices() -> Vec<&'static Px4Model> {
    Vec::new()
}

const DTV_BUILDS_CONTENTS_URL: &str =
    "https://api.github.com/repos/tsukumijima/DTV-Builds/contents/";

/// 実際のダウンロード・展開・インストール処理。ネットワークアクセスが必要なため
/// `webhook` フィーチャー(reqwestを有効化するフィーチャー、デフォルト有効)が
/// 必要。無効ビルドでは分かりやすいエラーを返す [`unsupported`] 実装に切り替わる。
#[cfg(feature = "webhook")]
mod imp {
    use super::*;

    /// DTV-Builds リポジトリから最新の px4_drv_winusb 配布物のファイル名とダウンロードURLを取得する。
    fn fetch_latest_release_asset() -> Result<(String, String), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("recisdb-proxy-setup")
            .build()
            .map_err(|e| e.to_string())?;

        let resp = client
            .get(DTV_BUILDS_CONTENTS_URL)
            .send()
            .map_err(|e| format!("GitHubへの問い合わせに失敗しました: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!(
                "GitHubへの問い合わせに失敗しました (HTTP {})",
                resp.status()
            ));
        }

        let body: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
        let items = body
            .as_array()
            .ok_or_else(|| "GitHub APIの応答が予期しない形式でした".to_string())?;

        let mut candidates: Vec<(String, String)> = items
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                if name.starts_with("px4_drv_winusb-") && name.ends_with(".zip") {
                    let url = item.get("download_url")?.as_str()?;
                    Some((name.to_string(), url.to_string()))
                } else {
                    None
                }
            })
            .collect();

        // ファイル名の末尾が YYMMDD なので文字列順ソートで最新版が末尾に来る。
        candidates.sort_by(|a, b| a.0.cmp(&b.0));
        candidates
            .into_iter()
            .last()
            .ok_or_else(|| "px4_drv_winusbの配布ファイルが見つかりませんでした".to_string())
    }

    fn download_zip(url: &str, dest_zip: &Path) -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("recisdb-proxy-setup")
            .build()
            .map_err(|e| e.to_string())?;

        let mut resp = client
            .get(url)
            .send()
            .map_err(|e| format!("ダウンロードに失敗しました: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("ダウンロードに失敗しました (HTTP {})", resp.status()));
        }

        let mut file = std::fs::File::create(dest_zip).map_err(|e| e.to_string())?;
        resp.copy_to(&mut file).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
        let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        archive
            .extract(dest_dir)
            .map_err(|e| format!("展開に失敗しました: {e}"))?;

        std::fs::read_dir(dest_dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && e.file_name().to_string_lossy().starts_with("px4_drv_winusb")
            })
            .map(|e| e.path())
            .ok_or_else(|| "展開後のフォルダが見つかりませんでした".to_string())
    }

    /// ドライバのインストール (署名用証明書の登録 + .inf のインストール) を
    /// 管理者権限で実行する。UACの確認画面が表示される。
    #[cfg(target_os = "windows")]
    fn install_driver_elevated(extracted_root: &Path, model: &Px4Model) -> Result<(), String> {
        let cert_install = extracted_root.join("Driver").join("cert-install.jse");
        let inf_path = extracted_root.join("Driver").join(model.inf_name);

        if !cert_install.exists() || !inf_path.exists() {
            return Err(
                "ドライバファイルが見つかりません(展開に失敗した可能性があります)".to_string(),
            );
        }

        let script_path = std::env::temp_dir().join("recisdb-proxy-px4-install.ps1");
        let script = format!(
            "$ErrorActionPreference = 'Stop'\r\n\
             & cscript.exe //Nologo \"{cert}\"\r\n\
             & pnputil.exe /add-driver \"{inf}\" /install\r\n\
             exit $LASTEXITCODE\r\n",
            cert = cert_install.display(),
            inf = inf_path.display(),
        );
        std::fs::write(&script_path, script).map_err(|e| e.to_string())?;

        // 呼び出し元プロセス自体は昇格させず、内側の powershell だけを
        // `-Verb RunAs` で昇格させる(UACの確認画面がここだけに出る)。
        let elevate_cmd = format!(
            "$p = Start-Process powershell -Verb RunAs -Wait -PassThru \
             -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'; \
             exit $p.ExitCode",
            script_path.display()
        );

        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &elevate_cmd])
            .status()
            .map_err(|e| e.to_string())?;

        match status.code() {
            Some(0) => Ok(()),
            Some(1223) => Err("管理者権限の許可がキャンセルされました。もう一度お試しください。".to_string()),
            code => Err(format!("ドライバのインストールに失敗しました (終了コード: {code:?})")),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn install_driver_elevated(_extracted_root: &Path, _model: &Px4Model) -> Result<(), String> {
        Err("ドライバの自動インストールはWindows専用の機能です。".to_string())
    }

    /// 展開済みアーカイブから、対応するBonDriver一式をインストール先フォルダにコピーする。
    fn stage_bondriver(
        extracted_root: &Path,
        model: &Px4Model,
        install_dir: &Path,
    ) -> Result<Vec<String>, String> {
        let bitness = if cfg!(target_pointer_width = "64") {
            "64bit"
        } else {
            "32bit"
        };
        let src = extracted_root.join(format!("{}_{}", model.bondriver_folder, bitness));
        if !src.exists() {
            return Err(format!("{} が見つかりません", src.display()));
        }

        let dest = install_dir.join("BonDriver").join(model.bondriver_folder);
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

        for entry in std::fs::read_dir(&src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                std::fs::copy(entry.path(), dest.join(entry.file_name()))
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(model
            .dll_names
            .iter()
            .map(|name| dest.join(name).to_string_lossy().to_string())
            .collect())
    }

    /// ダウンロード・展開・ドライバインストール・BonDriver配置までを一括で行う。
    /// `on_progress` は各段階の進捗メッセージ通知用のコールバック。
    pub fn download_install_and_stage(
        usb_pid: u16,
        install_dir: &Path,
        mut on_progress: impl FnMut(&str),
    ) -> Result<Vec<String>, String> {
        let model = find_model(usb_pid).ok_or_else(|| "対応していないデバイスです".to_string())?;

        on_progress(&format!("{} 用ドライバの最新版を確認しています…", model.label));
        let (asset_name, url) = fetch_latest_release_asset()?;

        let cache_dir = install_dir.join("drivers").join("px4_drv_winusb");
        std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
        let extracted_root_guess = cache_dir.join(asset_name.trim_end_matches(".zip"));

        let extracted_root = if extracted_root_guess.exists() {
            on_progress("ダウンロード済みのファイルを使用します…");
            extracted_root_guess
        } else {
            on_progress(&format!("{asset_name} をダウンロードしています…"));
            let zip_path = cache_dir.join(&asset_name);
            download_zip(&url, &zip_path)?;

            on_progress("展開しています…");
            let root = extract_zip(&zip_path, &cache_dir)?;
            let _ = std::fs::remove_file(&zip_path);
            root
        };

        on_progress("ドライバをインストールしています(管理者権限の確認画面が表示されます)…");
        install_driver_elevated(&extracted_root, model)?;

        on_progress("BonDriverを配置しています…");
        let paths = stage_bondriver(&extracted_root, model, install_dir)?;

        on_progress("完了しました。");
        Ok(paths)
    }

    #[cfg(test)]
    mod network_tests {
        use super::*;

        /// 実ネットワーク・実ファイルシステムを使う統合テスト。通常のテスト実行
        /// では走らない (`cargo test -- --ignored` で手動実行する)。
        /// ドライバインストール(要管理者権限・UAC)は含めず、ダウンロードと
        /// 展開だけを検証する。
        #[test]
        #[ignore]
        fn download_and_extract_latest_px4_drv_winusb() {
            let (asset_name, url) = fetch_latest_release_asset().expect("asset lookup should succeed");
            assert!(asset_name.starts_with("px4_drv_winusb-"));
            assert!(asset_name.ends_with(".zip"));

            let dir = std::env::temp_dir().join("recisdb-proxy-px4-installer-test");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            let zip_path = dir.join(&asset_name);
            download_zip(&url, &zip_path).expect("download should succeed");
            assert!(zip_path.metadata().unwrap().len() > 0);

            let root = extract_zip(&zip_path, &dir).expect("extract should succeed");
            assert!(root.join("Driver").join("PX-W3U4.inf").exists());
            assert!(root.join("Driver").join("cert-install.jse").exists());
            assert!(root.join("BonDriver_PX4_64bit").join("BonDriver_PX4-T.dll").exists());

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(not(feature = "webhook"))]
mod imp {
    use super::*;

    pub fn download_install_and_stage(
        _usb_pid: u16,
        _install_dir: &Path,
        _on_progress: impl FnMut(&str),
    ) -> Result<Vec<String>, String> {
        Err(
            "この機能を使うには webhook フィーチャーを有効にしてビルドしてください \
             (デフォルトのビルドでは有効です)。"
                .to_string(),
        )
    }
}

pub use imp::download_install_and_stage;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_model_matches_known_pid() {
        let model = find_model(0x083f).expect("PX-W3U4 should be a known model");
        assert_eq!(model.label, "PLEX PX-W3U4");
        assert_eq!(model.bondriver_folder, "BonDriver_PX4");
        assert_eq!(model.dll_names, &["BonDriver_PX4-S.dll", "BonDriver_PX4-T.dll"]);
    }

    #[test]
    fn find_model_returns_none_for_unknown_pid() {
        assert!(find_model(0xffff).is_none());
    }

    #[test]
    fn all_models_have_unique_pids() {
        let mut pids: Vec<u16> = PX4_MODELS.iter().map(|m| m.usb_pid).collect();
        let len_before = pids.len();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), len_before, "duplicate usb_pid in PX4_MODELS");
    }
}
