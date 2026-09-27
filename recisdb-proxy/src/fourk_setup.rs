//! Windows BS4Kセットアップの純粋ロジックと配布物取得。
//!
//! UIから切り離して、リリースJSON・DLL名・INI・カードリーダー出力を
//! 単体テストできる形にする。実際のダウンロードとファイル配置は、GUIの
//! ワーカースレッドからだけ呼び出す。

use std::path::{Path, PathBuf};

pub const DANTTO_RELEASES_API: &str =
    "https://api.github.com/repos/nekohkr/dantto4k/releases?per_page=10";
pub const TSREPLACE_RELEASE_API: &str =
    "https://api.github.com/repos/rigaya/tsreplace/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcasSelection {
    Automatic,
    SmartCardReader(String),
    CasProxyServer(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanttoReleaseAsset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWrapper {
    pub base_path: PathBuf,
    pub wrapper_path: PathBuf,
}

/// `tsreplace_1.2.3_x64.7z` だけを採用する。
pub fn select_tsreplace_asset(json: &str) -> Option<DanttoReleaseAsset> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let assets = value.get("assets")?.as_array()?;
    assets.iter().find_map(|asset| {
        let name = asset.get("name")?.as_str()?;
        let url = asset.get("browser_download_url")?.as_str()?;
        is_tsreplace_x64_asset(name).then(|| DanttoReleaseAsset {
            name: name.to_owned(),
            url: url.to_owned(),
        })
    })
}

fn is_tsreplace_x64_asset(name: &str) -> bool {
    let Some(version) = name
        .strip_prefix("tsreplace_")
        .and_then(|s| s.strip_suffix("_x64.7z"))
    else {
        return false;
    };
    !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// 最新順に並ぶGitHub releasesの先頭から、Windows x64 assetを持つ版を選ぶ。
pub fn select_dantto_release_asset(json: &str) -> Option<DanttoReleaseAsset> {
    let releases: serde_json::Value = serde_json::from_str(json).ok()?;
    let releases = releases.as_array()?;

    for preferred in [true, false] {
        for release in releases {
            let Some(assets) = release.get("assets").and_then(|v| v.as_array()) else {
                continue;
            };
            if let Some(asset) = assets.iter().find_map(|asset| {
                let name = asset.get("name")?.as_str()?;
                let is_zip = name.starts_with("dantto4k-") && name.ends_with(".zip");
                let is_preferred = is_zip && name.ends_with("windows-x64.zip");
                let matches = if preferred { is_preferred } else { is_zip };
                let url = asset.get("browser_download_url")?.as_str()?;
                matches.then(|| DanttoReleaseAsset {
                    name: name.to_owned(),
                    url: url.to_owned(),
                })
            }) {
                return Some(asset);
            }
        }
    }
    None
}

/// 基底BonDriver名から、同じフォルダに置くラッパー名を作る。
pub fn wrapper_dll_name(base_path: &Path) -> Option<String> {
    let file_name = base_path.file_name()?.to_str()?;
    let stem = file_name.get(..file_name.len().checked_sub(4)?)?;
    if !file_name.ends_with(".dll") && !file_name.ends_with(".DLL") {
        return None;
    }
    let suffix = stem.strip_prefix("BonDriver_")?;
    (!suffix.is_empty()).then(|| format!("BonDriver_dantto4k_{suffix}.dll"))
}

/// dantto4kのINIを生成または更新する。未知のキーとコメントは保持する。
pub fn update_dantto_ini(
    original: Option<&str>,
    bondriver_path: &str,
    acas: &AcasSelection,
) -> String {
    let mut lines: Vec<String> = original
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    if lines.is_empty() {
        lines = vec![
            "[bondriver]".to_owned(),
            format!("bondriverPath={bondriver_path}"),
            "".to_owned(),
            "[acas]".to_owned(),
            "smartCardReaderName=".to_owned(),
            "casProxyServer=".to_owned(),
            "".to_owned(),
            "[audio]".to_owned(),
            "disableADTSConversion=false".to_owned(),
        ];
    }

    set_ini_key(&mut lines, "bondriver", "bondriverPath", bondriver_path);
    let (reader, proxy) = match acas {
        AcasSelection::Automatic => ("", ""),
        AcasSelection::SmartCardReader(value) => (value.trim(), ""),
        AcasSelection::CasProxyServer(value) => ("", value.trim()),
    };
    set_ini_key(&mut lines, "acas", "smartCardReaderName", reader);
    set_ini_key(&mut lines, "acas", "casProxyServer", proxy);

    let mut result = lines.join("\r\n");
    result.push_str("\r\n");
    result
}

fn set_ini_key(lines: &mut Vec<String>, section: &str, key: &str, value: &str) {
    let section_header = format!("[{section}]");
    let Some(section_start) = lines.iter().position(|line| line.trim() == section_header) else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(section_header);
        lines.push(format!("{key}={value}"));
        return;
    };

    let section_end = lines
        .iter()
        .enumerate()
        .skip(section_start + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    if let Some(index) = (section_start + 1..section_end).find(|&index| {
        let trimmed = lines[index].trim_start();
        trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=')
    }) {
        let prefix = lines[index].split('=').next().unwrap_or(key).trim_end();
        lines[index] = format!("{prefix}={value}");
    } else {
        lines.insert(section_end, format!("{key}={value}"));
    }
}

/// dantto4k `--listSmartCardReader` の出力から候補名を取り出す。
pub fn parse_smart_card_readers(output: &str) -> Vec<String> {
    let mut readers = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let line = line.strip_prefix("-").map(str::trim).unwrap_or(line);
        if !line.is_empty() && !readers.iter().any(|known| known == line) {
            readers.push(line.to_owned());
        }
    }
    readers
}

#[cfg(feature = "webhook")]
fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("recisdb-proxy-setup")
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(feature = "webhook")]
fn get_json(url: &str) -> Result<String, String> {
    let response = http_client()?
        .get(url)
        .send()
        .map_err(|e| format!("GitHubへの問い合わせに失敗しました: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHubへの問い合わせに失敗しました (HTTP {})",
            response.status()
        ));
    }
    response.text().map_err(|e| e.to_string())
}

#[cfg(feature = "webhook")]
fn download_file(url: &str, path: &Path) -> Result<(), String> {
    let mut response = http_client()?
        .get(url)
        .send()
        .map_err(|e| format!("ダウンロードに失敗しました: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "ダウンロードに失敗しました (HTTP {})",
            response.status()
        ));
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    response.copy_to(&mut file).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(feature = "webhook")]
fn extract_seven_z(archive: &Path, destination: &Path) -> Result<(), String> {
    sevenz_rust2::decompress_file(archive, destination)
        .map_err(|e| format!("7zの展開に失敗しました: {e}"))
}

#[cfg(feature = "webhook")]
fn find_file_recursive(root: &Path, file_name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(file_name) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(feature = "webhook")]
fn ensure_tsreplace_package(
    install_dir: &Path,
    on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    let destination = install_dir.join("thirdparty").join("tsreplace");
    std::fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
    if let Some(path) = find_file_recursive(&destination, "tsreplace.exe") {
        return Ok(path);
    }

    on_progress("tsreplaceの最新版を確認しています…");
    let release = get_json(TSREPLACE_RELEASE_API)?;
    let asset = select_tsreplace_asset(&release)
        .ok_or_else(|| "x64版tsreplaceの配布ファイルが見つかりませんでした".to_owned())?;
    let archive = destination.join(&asset.name);
    on_progress(&format!("{}をダウンロードしています…", asset.name));
    download_file(&asset.url, &archive)?;
    on_progress("tsreplaceを展開しています…");
    extract_seven_z(&archive, &destination)?;
    find_file_recursive(&destination, "tsreplace.exe")
        .ok_or_else(|| "展開後にtsreplace.exeが見つかりませんでした".to_owned())
}

#[cfg(not(feature = "webhook"))]
fn ensure_tsreplace_package(
    _install_dir: &Path,
    _on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    Err("tsreplaceの自動取得にはwebhookフィーチャーが必要です".to_owned())
}

/// 管理フォルダまたはPATHからtsreplaceを得る。Windowsでは未検出時に自動取得。
pub fn ensure_tsreplace(
    install_dir: &Path,
    on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) {
        "tsreplace.exe"
    } else {
        "tsreplace"
    };
    let managed = install_dir
        .join("thirdparty")
        .join("tsreplace")
        .join(exe_name);
    if managed.is_file() {
        return Ok(managed);
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe_name))
            .find(|path| path.is_file())
    }) {
        return Ok(path);
    }
    if cfg!(all(windows, target_pointer_width = "64")) {
        ensure_tsreplace_package(install_dir, on_progress)
    } else {
        Err("tsreplaceの自動取得はWindows x64専用です。PATHへ配置してください".to_owned())
    }
}

#[cfg(feature = "webhook")]
fn ensure_dantto_package(
    install_dir: &Path,
    on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    let destination = install_dir.join("thirdparty").join("dantto4k");
    std::fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
    if let Some(path) = find_file_recursive(&destination, "BonDriver_dantto4k.dll") {
        return Ok(path);
    }
    on_progress("dantto4kの最新版を確認しています…");
    let releases = get_json(DANTTO_RELEASES_API)?;
    let asset = select_dantto_release_asset(&releases)
        .ok_or_else(|| "dantto4kのWindows配布ファイルが見つかりませんでした".to_owned())?;
    let archive = destination.join(&asset.name);
    on_progress(&format!("{}をダウンロードしています…", asset.name));
    download_file(&asset.url, &archive)?;
    on_progress("dantto4kを展開しています…");
    let file = std::fs::File::open(&archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    zip.extract(&destination)
        .map_err(|e| format!("zipの展開に失敗しました: {e}"))?;
    find_file_recursive(&destination, "BonDriver_dantto4k.dll")
        .ok_or_else(|| "展開後にBonDriver_dantto4k.dllが見つかりませんでした".to_owned())
}

#[cfg(not(feature = "webhook"))]
fn ensure_dantto_package(
    _install_dir: &Path,
    _on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    Err("dantto4kの自動取得にはwebhookフィーチャーが必要です".to_owned())
}

pub fn ensure_dantto_package_for_setup(
    install_dir: &Path,
    on_progress: &mut impl FnMut(&str),
) -> Result<PathBuf, String> {
    ensure_dantto_package(install_dir, on_progress)
}

#[cfg(feature = "webhook")]
pub fn find_dantto_executable(install_dir: &Path) -> Option<PathBuf> {
    find_file_recursive(
        &install_dir.join("thirdparty").join("dantto4k"),
        "dantto4k.exe",
    )
}

#[cfg(not(feature = "webhook"))]
pub fn find_dantto_executable(_install_dir: &Path) -> Option<PathBuf> {
    None
}

/// 基底DLLごとにラッパーとINIを作る。ラッパーは基底DLLと同じフォルダに置く。
pub fn prepare_wrappers(
    install_dir: &Path,
    base_paths: &[PathBuf],
    acas: &AcasSelection,
    on_progress: &mut impl FnMut(&str),
) -> Result<Vec<PreparedWrapper>, String> {
    let bundled = ensure_dantto_package(install_dir, on_progress)?;
    let mut result = Vec::new();
    for base in base_paths {
        let base = if base.is_absolute() {
            base.clone()
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(base)
        };
        if !base.is_file() {
            return Err(format!("基底BonDriverが見つかりません: {}", base.display()));
        }
        let wrapper_name = wrapper_dll_name(&base)
            .ok_or_else(|| format!("BonDriver DLL名を解決できません: {}", base.display()))?;
        let wrapper = base.parent().unwrap_or(Path::new(".")).join(wrapper_name);
        if wrapper.to_string_lossy().len() > 250 {
            return Err(format!(
                "ラッパーDLLのパスが長すぎます(250文字以内): {}",
                wrapper.display()
            ));
        }
        std::fs::copy(&bundled, &wrapper)
            .map_err(|e| format!("ラッパーDLLの配置に失敗しました: {e}"))?;
        let ini = wrapper.with_extension("ini");
        let original = std::fs::read_to_string(&ini).ok();
        let content = update_dantto_ini(original.as_deref(), &base.to_string_lossy(), acas);
        std::fs::write(&ini, content)
            .map_err(|e| format!("dantto4k INIの保存に失敗しました: {e}"))?;
        result.push(PreparedWrapper {
            base_path: base,
            wrapper_path: wrapper,
        });
    }
    Ok(result)
}

#[cfg(windows)]
pub fn list_smart_card_readers(exe: &Path) -> Result<Vec<String>, String> {
    use std::os::windows::process::CommandExt;
    let mut child = std::process::Command::new(exe)
        .arg("--listSmartCardReader")
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("カードリーダー一覧の取得に失敗しました: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut readers = parse_smart_card_readers(&stdout);
            readers.extend(parse_smart_card_readers(&stderr));
            readers.sort();
            readers.dedup();
            return Ok(readers);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("カードリーダー一覧の取得が5秒で終了しませんでした".to_owned());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
pub fn list_smart_card_readers(_exe: &Path) -> Result<Vec<String>, String> {
    Err("カードリーダー一覧はWindows専用です".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_versioned_tsreplace_x64_asset() {
        let json = r#"{"assets":[
            {"name":"tsreplace_1.2_x64.7z","browser_download_url":"a"},
            {"name":"tsreplace_latest_x64.7z","browser_download_url":"b"},
            {"name":"tsreplace_1.2.3_x86.7z","browser_download_url":"c"},
            {"name":"tsreplace_1.2.3_x64.7z","browser_download_url":"d"}
        ]}"#;
        assert_eq!(
            select_tsreplace_asset(json).unwrap().name,
            "tsreplace_1.2_x64.7z"
        );
    }

    #[test]
    fn selects_preferred_dantto_asset_from_first_matching_release() {
        let json = r#"[
          {"assets":[{"name":"dantto4k-v1.0.2-rc2-windows-x64.zip","browser_download_url":"new"}]},
          {"assets":[{"name":"dantto4k-v1.0.1.zip","browser_download_url":"old"}]}
        ]"#;
        assert_eq!(select_dantto_release_asset(json).unwrap().url, "new");
    }

    #[test]
    fn derives_wrapper_name() {
        assert_eq!(
            wrapper_dll_name(Path::new("BonDriver_BDA.dll")),
            Some("BonDriver_dantto4k_BDA.dll".to_owned())
        );
        assert_eq!(wrapper_dll_name(Path::new("other.dll")), None);
    }

    #[test]
    fn updates_ini_and_keeps_unknown_lines() {
        let original =
            "; keep\n[bondriver]\nbondriverPath=old\ncustom=x\n\n[acas]\nsmartCardReaderName=old\n";
        let result = update_dantto_ini(
            Some(original),
            r"C:\\DTV\\base.dll",
            &AcasSelection::CasProxyServer("127.0.0.1:24000".into()),
        );
        assert!(result.contains("; keep"));
        assert!(result.contains(r"bondriverPath=C:\\DTV\\base.dll"));
        assert!(result.contains("custom=x"));
        assert!(result.contains("casProxyServer=127.0.0.1:24000"));
        assert!(result.contains("smartCardReaderName="));
    }

    #[test]
    fn parses_reader_lines_and_deduplicates() {
        assert_eq!(
            parse_smart_card_readers("Reader A\n- Reader B\nReader A\n"),
            vec!["Reader A", "Reader B"]
        );
    }
}
