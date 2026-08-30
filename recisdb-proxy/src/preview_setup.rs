//! Automatic setup for the browser-preview pipeline (`?profile=preview`,
//! STREAMING_DESIGN.md §6.3/§9 P5).
//!
//! Before this module existed, `[preview] command_path`/`preprocessor_path`
//! had to be hand-configured (QSVEncC + tsreadex), and `?profile=preview`
//! returned 503 until then (`web/stream.rs::load_preview_encoder_config`).
//! [`ensure_preview_ready`] automates that: it locates (or downloads) an
//! encoder and a preprocessor, wires them into the DB *and* the TOML config
//! file, and flips `preview_encoder_config.enabled`.
//!
//! # Why ffmpeg instead of QSVEncC
//! The `encode_profiles` seed (`database::encode_profile::DEFAULT_PREVIEW_ENCODE_ARGS`)
//! still targets QSVEncC for anyone who wires it up by hand, but this module
//! standardizes on ffmpeg: QSVEncC/NVEncC/rigaya's encoders are distributed
//! per OS/GPU combination as `.7z`/`.deb` archives, which would multiply the
//! extraction logic here for no real benefit — ffmpeg ships a single
//! portable build per OS/arch and already bundles every hardware backend
//! (`h264_qsv`, `h264_nvenc`, `h264_amf`, `h264_vaapi`, `h264_videotoolbox`)
//! behind one `-c:v` flag.
//!
//! # Security (REVIEW_2026-07.md S1, same rule as `[preview] command_path`)
//! Nothing in this module accepts an executable path from an HTTP request
//! body. Every path written to the DB/TOML here is either a path this
//! module detected on disk or a path this module itself downloaded to a
//! directory it controls (`<install_dir>/thirdparty/...`). The HTTP handler
//! wrapping this (`web/api/configs.rs::auto_setup_preview`) takes no
//! request body at all.

use std::path::{Path, PathBuf};

use crate::database::Database;

/// Result of an [`ensure_preview_ready`] run, returned to the dashboard so
/// it can show the operator what actually got configured.
#[derive(Debug, Clone)]
pub struct PreviewSetupReport {
    pub enabled: bool,
    pub encoder_path: String,
    /// `"detected"` (already present), `"downloaded"` (BtbN static build),
    /// or `"homebrew"` (macOS, via `brew install ffmpeg`).
    pub encoder_source: String,
    /// ffmpeg `-c:v` name actually selected (`h264_videotoolbox`,
    /// `h264_qsv`, `h264_nvenc`, `h264_amf`, `h264_vaapi`, or the `libx264`
    /// software fallback).
    pub video_encoder: String,
    /// Empty when tsreadex could not be found/built — preview still works
    /// without it (no per-service caption conversion), see `warnings`.
    pub preprocessor_path: String,
    pub warnings: Vec<String>,
}

// ============================================================================
// Small cross-platform helpers shared by the ffmpeg/tsreadex paths below.
// ============================================================================

fn exe_file_name(base: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Linear `PATH` search, since adding a `which`-style crate dependency is
/// unnecessary for a single directory scan (constraint: no new crates).
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Recursive filename search under `dir`, used to locate the binary inside
/// a just-extracted archive without needing to know its exact subpath.
#[cfg(feature = "webhook")]
fn find_file_named(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
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

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).map_err(|e| e.to_string())?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Download/extract helpers shared by the ffmpeg and tsreadex fetchers.
#[cfg(feature = "webhook")]
mod fetch {
    use super::*;

    pub(super) fn http_client() -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .user_agent("recisdb-proxy-setup")
            .build()
            .map_err(|e| e.to_string())
    }

    pub(super) fn download_file(url: &str, dest: &Path) -> Result<(), String> {
        let client = http_client()?;
        let mut resp = client.get(url).send().map_err(|e| format!("ダウンロードに失敗しました: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("ダウンロードに失敗しました (HTTP {})", resp.status()));
        }
        let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        resp.copy_to(&mut file).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Fetches `url` and parses it as JSON, used for the GitHub release APIs.
    pub(super) fn get_json(url: &str) -> Result<serde_json::Value, String> {
        let client = http_client()?;
        let resp = client
            .get(url)
            .send()
            .map_err(|e| format!("GitHubへの問い合わせに失敗しました: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GitHubへの問い合わせに失敗しました (HTTP {})", resp.status()));
        }
        resp.json().map_err(|e| e.to_string())
    }

    pub(super) fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
        let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        archive.extract(dest_dir).map_err(|e| format!("展開に失敗しました: {e}"))
    }

    /// `flag` is the `tar` extraction flag including the compression letter
    /// (`-xJf` for xz, `-xzf` for gzip).
    ///
    /// No `xz`/`tar` crate in the dependency graph; shell out to the system
    /// `tar`, which supports both on every macOS/Linux this project targets
    /// (constraint: no new crate dependencies).
    pub(super) fn extract_tar(archive_path: &Path, dest_dir: &Path, flag: &str) -> Result<(), String> {
        let status = std::process::Command::new("tar")
            .arg(flag)
            .arg(archive_path)
            .arg("-C")
            .arg(dest_dir)
            .status()
            .map_err(|e| format!("tar の実行に失敗しました: {e}"))?;
        if !status.success() {
            return Err(format!("tar の展開に失敗しました (終了コード: {:?})", status.code()));
        }
        Ok(())
    }

    /// Extracts by archive shape: `.zip` via the `zip` crate, `.tar.xz` /
    /// `.tar.gz` via the system `tar`.
    pub(super) fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
        let name = archive_path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.ends_with(".zip") {
            extract_zip(archive_path, dest_dir)
        } else if name.ends_with(".tar.xz") {
            extract_tar(archive_path, dest_dir, "-xJf")
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            extract_tar(archive_path, dest_dir, "-xzf")
        } else {
            Err(format!("対応していない書庫形式です: {name}"))
        }
    }
}

// ============================================================================
// ffmpeg: detection
// ============================================================================

fn managed_ffmpeg_path(install_dir: &Path) -> PathBuf {
    install_dir.join("thirdparty").join("ffmpeg").join(exe_file_name("ffmpeg"))
}

/// KonomiTV bundles its own ffmpeg build; reuse it if present rather than
/// downloading a second copy. KonomiTV does not ship a macOS build.
fn konomitv_ffmpeg_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(PathBuf::from(r"C:\DTV\KonomiTV\server\thirdparty\FFmpeg\bin\ffmpeg.exe"))
    } else if cfg!(target_os = "linux") {
        Some(PathBuf::from("/opt/KonomiTV/server/thirdparty/FFmpeg/bin/ffmpeg"))
    } else {
        None
    }
}

/// Detection order: this module's own managed copy, then KonomiTV's bundled
/// copy, then `PATH`. Does not verify the binary actually runs — see
/// [`verify_ffmpeg_binary`].
fn detect_ffmpeg(install_dir: &Path) -> Option<PathBuf> {
    let managed = managed_ffmpeg_path(install_dir);
    if managed.is_file() {
        return Some(managed);
    }
    if let Some(path) = konomitv_ffmpeg_path() {
        if path.is_file() {
            return Some(path);
        }
    }
    which_on_path(exe_file_name("ffmpeg").as_str())
}

fn run_ffmpeg_encoders(ffmpeg_path: &Path) -> Result<String, String> {
    let output = std::process::Command::new(ffmpeg_path)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|e| format!("ffmpeg の実行に失敗しました ({}): {e}", ffmpeg_path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg -encoders が失敗しました (終了コード: {:?})",
            output.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A path existing is not enough — a broken binary, or an unrelated program
/// that happens to share the name `ffmpeg`, must not be accepted silently.
/// Runs `-encoders` and requires `libx264` (always built into every real
/// ffmpeg distribution) to appear in the output.
fn verify_ffmpeg_binary(ffmpeg_path: &Path) -> Result<String, String> {
    let encoders = run_ffmpeg_encoders(ffmpeg_path)?;
    if !encoders.contains("libx264") {
        return Err(format!(
            "{} は libx264 エンコーダを含んでいません(壊れているか、ffmpegとは別のコマンドです)",
            ffmpeg_path.display()
        ));
    }
    Ok(encoders)
}

// ============================================================================
// ffmpeg: hardware encoder selection
// ============================================================================

/// The three platform buckets that matter for hardware-encoder priority.
/// A plain enum (rather than branching on `cfg!` everywhere) so the
/// selection logic can be unit-tested for all three without needing to
/// actually run on each OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostOs {
    MacOs,
    Windows,
    Linux,
}

fn current_host_os() -> HostOs {
    if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else if cfg!(target_os = "windows") {
        HostOs::Windows
    } else {
        HostOs::Linux
    }
}

/// Hardware encoder candidates in priority order for `os`. The final
/// fallback (`libx264`, software) is deliberately not in this list — it is
/// applied by the caller when none of these pan out.
fn hardware_encoder_candidates(os: HostOs) -> &'static [&'static str] {
    match os {
        HostOs::MacOs => &["h264_videotoolbox"],
        HostOs::Windows => &["h264_qsv", "h264_nvenc", "h264_amf"],
        HostOs::Linux => &["h264_qsv", "h264_nvenc", "h264_vaapi"],
    }
}

/// Candidates (in priority order) that this ffmpeg build actually lists in
/// its `-encoders` output. Being *listed* only means the ffmpeg build
/// includes the encoder, not that a working GPU backs it — a headless
/// server with a GPU-enabled ffmpeg build will still list `h264_qsv` with
/// no QSV hardware present. [`select_working_video_encoder`] additionally
/// probes each of these with a short test-encode before accepting one.
fn listed_hardware_encoders(os: HostOs, encoders_output: &str) -> Vec<&'static str> {
    hardware_encoder_candidates(os)
        .iter()
        .copied()
        .filter(|name| encoders_output.contains(name))
        .collect()
}

/// One-second-ish smoke test: does this encoder actually produce output, or
/// does it fail because the hardware/driver isn't really there?
///
/// **本番と同じ最適化オプション込みで試す。** `-c:v <名前>` だけ通ることを
/// 確認しても、本番の引数にしか出てこないオプション (`-tune ll` など) が
/// そのビルドで効かなければ、視聴を開始した瞬間に初めて落ちる。ここで一緒に
/// 試しておけば、その場合はこの候補を落として次 (最終的には libx264) へ回せる。
/// `h264_vaapi` のようにデバイス初期化が要るものも、ここで自然に脱落する。
fn test_encode_works(ffmpeg_path: &Path, video_encoder: &str) -> bool {
    let mut args: Vec<&str> = vec![
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=d=0.2",
        "-c:v",
        video_encoder,
    ];
    args.extend(crate::database::video_encoder_tuning(video_encoder).split_whitespace());
    args.extend(["-f", "null", "-"]);

    std::process::Command::new(ffmpeg_path)
        .args(&args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Picks the video encoder to use: the first candidate (OS-appropriate
/// priority order) that is both listed by this ffmpeg build *and* survives
/// a short test-encode, else the `libx264` software fallback (never fails,
/// so this function always returns something usable).
fn select_working_video_encoder(ffmpeg_path: &Path, encoders_output: &str) -> String {
    let os = current_host_os();
    listed_hardware_encoders(os, encoders_output)
        .into_iter()
        .find(|candidate| test_encode_works(ffmpeg_path, candidate))
        .unwrap_or("libx264")
        .to_string()
}

// ============================================================================
// ffmpeg: download (network — requires the `webhook` feature)
// ============================================================================

#[cfg(feature = "webhook")]
mod ffmpeg_download {
    use super::*;

    use super::fetch::{download_file, extract_archive};

    const BTBN_RELEASE_API: &str = "https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest";
    const BTBN_DOWNLOAD_BASE: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest";

    /// BtbN's static builds cover Windows/Linux x86_64/aarch64. No macOS
    /// build exists there, so macOS goes through Homebrew instead (see
    /// [`install_via_homebrew`]).
    ///
    /// Returns the platform slug used in BtbN asset names and the archive
    /// extension for it.
    fn btbn_platform() -> Result<(&'static str, &'static str), String> {
        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Ok(("win64", ".zip"))
        } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
            Ok(("winarm64", ".zip"))
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Ok(("linux64", ".tar.xz"))
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Ok(("linuxarm64", ".tar.xz"))
        } else {
            Err("この環境向けのffmpeg自動ダウンロードには対応していません".to_string())
        }
    }

    /// True for a *static* GPL build of `platform` — i.e. `-gpl` but not
    /// `-gpl-shared`, which needs the accompanying `lib/` directory and so
    /// cannot be copied out as a single binary.
    pub(super) fn is_static_gpl_asset(name: &str, prefix: &str, platform: &str, ext: &str) -> bool {
        let Some(rest) = name.strip_prefix(prefix) else { return false };
        let Some(rest) = rest.strip_suffix(ext) else { return false };
        // rest is e.g. "win64-gpl" or "win64-gpl-8.1" (release-branch builds
        // carry a version suffix) or "win64-gpl-shared".
        let Some(rest) = rest.strip_prefix(platform) else { return false };
        let Some(rest) = rest.strip_prefix("-gpl") else { return false };
        !rest.contains("shared")
    }

    /// Resolves the asset URL from the GitHub release itself rather than
    /// hardcoding a file name: BtbN renames assets across ffmpeg releases
    /// (`ffmpeg-n8.1-latest-win64-gpl.zip` became
    /// `ffmpeg-n8.1-latest-win64-gpl-8.1.zip`), and a stale hardcoded name
    /// fails with a bare HTTP 404.
    ///
    /// Prefers the newest release-branch (`n<major>.<minor>`) build, falling
    /// back to the `master` build. If the API is unreachable or rate-limited
    /// (unauthenticated GitHub API allows 60 requests/hour per IP), falls back
    /// to the `master-latest` name, whose shape has been stable.
    fn btbn_asset_url() -> Result<String, String> {
        let (platform, ext) = btbn_platform()?;
        let fallback = format!("{BTBN_DOWNLOAD_BASE}/ffmpeg-master-latest-{platform}-gpl{ext}");

        let Ok(body) = super::fetch::get_json(BTBN_RELEASE_API) else { return Ok(fallback) };
        let Some(assets) = body.get("assets").and_then(|v| v.as_array()) else { return Ok(fallback) };

        let mut best: Option<(String, String)> = None; // (version key, url)
        let mut master_url: Option<String> = None;
        for asset in assets {
            let Some(name) = asset.get("name").and_then(|v| v.as_str()) else { continue };
            let Some(url) = asset.get("browser_download_url").and_then(|v| v.as_str()) else { continue };

            if is_static_gpl_asset(name, "ffmpeg-master-latest-", platform, ext) {
                master_url = Some(url.to_string());
                continue;
            }
            // Release-branch builds: "ffmpeg-n<ver>-latest-<platform>-gpl...".
            let Some(rest) = name.strip_prefix("ffmpeg-n") else { continue };
            let Some((version, _)) = rest.split_once("-latest-") else { continue };
            let prefix = format!("ffmpeg-n{version}-latest-");
            if !is_static_gpl_asset(name, &prefix, platform, ext) {
                continue;
            }
            // Zero-pad each numeric component so "n8.1" sorts above "n10.0"
            // correctly under plain string comparison.
            let key: String = version.split('.').map(|part| format!("{part:0>4}.")).collect();
            if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                best = Some((key, url.to_string()));
            }
        }

        Ok(best.map(|(_, url)| url).or(master_url).unwrap_or(fallback))
    }

    fn install_via_homebrew() -> Result<PathBuf, String> {
        let brew = which_on_path("brew").ok_or_else(|| {
            "Homebrewが見つかりません。https://brew.sh からインストールしたうえで、\
             `brew install ffmpeg` を実行してください。"
                .to_string()
        })?;
        let status = std::process::Command::new(&brew)
            .args(["install", "ffmpeg"])
            .status()
            .map_err(|e| format!("`brew install ffmpeg` の実行に失敗しました: {e}"))?;
        if !status.success() {
            return Err(format!(
                "`brew install ffmpeg` が失敗しました (終了コード: {:?})",
                status.code()
            ));
        }
        which_on_path("ffmpeg")
            .ok_or_else(|| "`brew install ffmpeg` の後もffmpegがPATH上に見つかりませんでした".to_string())
    }

    /// Downloads (or, on macOS, `brew install`s) ffmpeg and returns its
    /// final path. Verification (`-encoders` / `libx264`) is the caller's
    /// job (`resolve_ffmpeg`), same as for a detected copy.
    pub fn download_ffmpeg(install_dir: &Path) -> Result<PathBuf, String> {
        if cfg!(target_os = "macos") {
            return install_via_homebrew();
        }

        let url = btbn_asset_url()?;
        let url = url.as_str();
        let dest_dir = install_dir.join("thirdparty").join("ffmpeg");
        std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;

        let tmp_dir = std::env::temp_dir().join(format!("recisdb-proxy-ffmpeg-dl-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
        let cleanup = || {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        };

        let is_zip = url.ends_with(".zip");
        let archive_path = tmp_dir.join(if is_zip { "ffmpeg.zip" } else { "ffmpeg.tar.xz" });
        if let Err(e) = download_file(url, &archive_path) {
            cleanup();
            return Err(e);
        }

        if let Err(e) = extract_archive(&archive_path, &tmp_dir) {
            cleanup();
            return Err(e);
        }

        let file_name = exe_file_name("ffmpeg");
        let Some(extracted) = find_file_named(&tmp_dir, &file_name) else {
            cleanup();
            return Err(format!("展開後に {file_name} が見つかりませんでした"));
        };

        let dest_file = dest_dir.join(&file_name);
        let copy_result = std::fs::copy(&extracted, &dest_file)
            .map_err(|e| e.to_string())
            .and_then(|_| make_executable(&dest_file));
        cleanup();
        copy_result?;
        Ok(dest_file)
    }
}

#[cfg(not(feature = "webhook"))]
mod ffmpeg_download {
    use super::*;

    pub fn download_ffmpeg(_install_dir: &Path) -> Result<PathBuf, String> {
        Err(
            "ffmpegの自動ダウンロードには webhook フィーチャーを有効にしてビルドしてください \
             (デフォルトのビルドでは有効です)。"
                .to_string(),
        )
    }
}

/// Detect an existing ffmpeg, verifying it works; if none is usable,
/// download one. Returns `(path, source_label, -encoders output)`.
fn resolve_ffmpeg(install_dir: &Path) -> Result<(PathBuf, &'static str, String), String> {
    if let Some(path) = detect_ffmpeg(install_dir) {
        if let Ok(encoders) = verify_ffmpeg_binary(&path) {
            return Ok((path, "detected", encoders));
        }
        // Detected but broken/unrelated — fall through to downloading a
        // known-good copy rather than erroring out immediately.
    }

    let downloaded = ffmpeg_download::download_ffmpeg(install_dir)?;
    let encoders = verify_ffmpeg_binary(&downloaded)?;
    let source = if cfg!(target_os = "macos") { "homebrew" } else { "downloaded" };
    Ok((downloaded, source, encoders))
}

// ============================================================================
// tsreadex: detection + fetch/build (optional stage-1 preprocessor)
// ============================================================================

fn managed_tsreadex_path(install_dir: &Path) -> PathBuf {
    install_dir.join("thirdparty").join("tsreadex").join(exe_file_name("tsreadex"))
}

fn konomitv_tsreadex_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(PathBuf::from(r"C:\DTV\KonomiTV\server\thirdparty\tsreadex\tsreadex.exe"))
    } else if cfg!(target_os = "linux") {
        Some(PathBuf::from("/opt/KonomiTV/server/thirdparty/tsreadex/tsreadex"))
    } else {
        None
    }
}

fn detect_tsreadex(install_dir: &Path) -> Option<PathBuf> {
    let managed = managed_tsreadex_path(install_dir);
    if managed.is_file() {
        return Some(managed);
    }
    if let Some(path) = konomitv_tsreadex_path() {
        if path.is_file() {
            return Some(path);
        }
    }
    which_on_path(exe_file_name("tsreadex").as_str())
}

#[cfg(feature = "webhook")]
mod tsreadex_setup {
    use super::*;

    use super::fetch::{download_file, extract_archive, extract_zip, get_json};

    const RELEASES_API: &str = "https://api.github.com/repos/xtne6f/tsreadex/releases?per_page=5";

    /// This project's own releases, which carry tsreadex builds for every
    /// platform we support (see `.github/workflows/tsreadex.yml`). Preferred
    /// over upstream because upstream ships Windows x86/x64 binaries only —
    /// every other platform would otherwise need a local C++ toolchain.
    const OWN_RELEASES_API: &str = "https://api.github.com/repos/stuayu/recisdb-proxy-rs/releases?per_page=15";

    struct ReleaseAsset {
        tag_name: String,
        asset_name: String,
        asset_url: String,
    }

    /// Finds the most recent release that ships a `tsreadex-*.zip` asset
    /// (the known real-world shape: `tsreadex-master-YYMMDD.zip`).
    fn fetch_latest_release_with_asset() -> Result<ReleaseAsset, String> {
        let body = get_json(RELEASES_API)?;
        let releases = body.as_array().ok_or_else(|| "GitHub APIの応答が予期しない形式でした".to_string())?;

        for release in releases {
            let Some(tag_name) = release.get("tag_name").and_then(|v| v.as_str()) else { continue };
            let Some(assets) = release.get("assets").and_then(|v| v.as_array()) else { continue };
            let asset = assets.iter().find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.starts_with("tsreadex-") && n.ends_with(".zip"))
                    .unwrap_or(false)
            });
            if let Some(asset) = asset {
                let asset_name = asset.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let asset_url = asset.get("browser_download_url").and_then(|u| u.as_str()).unwrap_or_default();
                if !asset_name.is_empty() && !asset_url.is_empty() {
                    return Ok(ReleaseAsset {
                        tag_name: tag_name.to_string(),
                        asset_name: asset_name.to_string(),
                        asset_url: asset_url.to_string(),
                    });
                }
            }
        }
        Err("tsreadexの配布ファイルが見つかりませんでした".to_string())
    }

    /// Asset label + archive extension this build looks for in our own
    /// releases. Must stay in sync with the matrix in
    /// `.github/workflows/tsreadex.yml`.
    ///
    /// Takes `(os, arch)` as reported by `std::env::consts` so the mapping
    /// stays unit-testable for platforms other than the host's.
    pub(super) fn own_asset_suffix(os: &str, arch: &str) -> Option<String> {
        let label = match (os, arch) {
            ("windows", "x86_64") => "win-x64",
            ("windows", "x86") => "win-x86",
            ("windows", "aarch64") => "win-arm64",
            ("linux", "x86_64") => "linux-amd64",
            ("linux", "aarch64") => "linux-arm64",
            ("macos", "x86_64") => "macos-amd64",
            ("macos", "aarch64") => "macos-arm64",
            _ => return None,
        };
        let ext = if os == "windows" { ".zip" } else { ".tar.gz" };
        Some(format!("-{label}{ext}"))
    }

    /// Newest `tsreadex-<tag>-<label>.<ext>` asset for this platform across
    /// our own releases. The API returns releases newest-first, so the first
    /// match wins.
    fn find_own_asset() -> Result<(String, String), String> {
        let suffix = own_asset_suffix(std::env::consts::OS, std::env::consts::ARCH)
            .ok_or_else(|| "この環境向けのtsreadexビルドは配布されていません".to_string())?;
        let body = get_json(OWN_RELEASES_API)?;
        let releases = body.as_array().ok_or_else(|| "GitHub APIの応答が予期しない形式でした".to_string())?;

        for release in releases {
            let Some(assets) = release.get("assets").and_then(|v| v.as_array()) else { continue };
            for asset in assets {
                let Some(name) = asset.get("name").and_then(|n| n.as_str()) else { continue };
                if !name.starts_with("tsreadex-") || !name.ends_with(&suffix) {
                    continue;
                }
                let Some(url) = asset.get("browser_download_url").and_then(|u| u.as_str()) else { continue };
                return Ok((name.to_string(), url.to_string()));
            }
        }
        Err(format!("tsreadexの配布ファイル (*{suffix}) が見つかりませんでした"))
    }

    /// Installs from our own prebuilt asset — a single archive holding
    /// `tsreadex-<tag>-<label>/tsreadex[.exe]` plus its license.
    fn install_from_own_release(tmp_dir: &Path, dest_file: &Path) -> Result<(), String> {
        let (asset_name, asset_url) = find_own_asset()?;
        let archive_path = tmp_dir.join(&asset_name);
        download_file(&asset_url, &archive_path)?;
        extract_archive(&archive_path, tmp_dir)?;

        let file_name = exe_file_name("tsreadex");
        let extracted =
            find_file_named(tmp_dir, &file_name).ok_or_else(|| format!("展開後に {file_name} が見つかりませんでした"))?;
        std::fs::copy(&extracted, dest_file).map_err(|e| e.to_string())?;
        make_executable(dest_file)
    }

    /// Windows: the release zip already contains prebuilt `tsreadex.exe`
    /// binaries (`x64/tsreadex.exe`, `x86/tsreadex.exe`) — just pick the one
    /// matching this build's pointer width.
    fn install_windows(release: &ReleaseAsset, tmp_dir: &Path, dest_file: &Path) -> Result<(), String> {
        let zip_path = tmp_dir.join(&release.asset_name);
        download_file(&release.asset_url, &zip_path)?;
        extract_zip(&zip_path, tmp_dir)?;

        // The zip holds both `x64/tsreadex.exe` and `x86/tsreadex.exe`;
        // `find_file_named` walks in unspecified order, so scan for the
        // matching bitness explicitly before falling back to any copy.
        let bitness_dir = if cfg!(target_pointer_width = "64") { "x64" } else { "x86" };
        let extracted = find_file_named(&tmp_dir.join(bitness_dir), "tsreadex.exe")
            .or_else(|| find_file_named(tmp_dir, "tsreadex.exe"))
            .ok_or_else(|| format!("展開後に {bitness_dir}/tsreadex.exe が見つかりませんでした"))?;
        std::fs::copy(&extracted, dest_file).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Unix: the release zip only contains Windows binaries, so fetch the
    /// tagged *source* archive instead and build it locally with `make`
    /// (verified to build cleanly, no extra dependencies, in a few seconds).
    fn install_unix(release: &ReleaseAsset, tmp_dir: &Path, dest_file: &Path) -> Result<(), String> {
        which_on_path("make").ok_or_else(|| "make が見つかりません(tsreadexのビルドをスキップします)".to_string())?;

        let src_url = format!(
            "https://github.com/xtne6f/tsreadex/archive/refs/tags/{}.zip",
            release.tag_name
        );
        let zip_path = tmp_dir.join("tsreadex-src.zip");
        download_file(&src_url, &zip_path)?;
        extract_zip(&zip_path, tmp_dir)?;

        let makefile = find_file_named(tmp_dir, "Makefile").ok_or_else(|| "展開後にMakefileが見つかりませんでした".to_string())?;
        let src_dir = makefile.parent().ok_or_else(|| "Makefileの親ディレクトリを特定できませんでした".to_string())?;

        let status = std::process::Command::new("make")
            .current_dir(src_dir)
            .status()
            .map_err(|e| format!("make の実行に失敗しました: {e}"))?;
        if !status.success() {
            return Err(format!("tsreadexのビルドに失敗しました (make 終了コード: {:?})", status.code()));
        }

        let built = src_dir.join("tsreadex");
        if !built.is_file() {
            return Err("ビルド後にtsreadex実行ファイルが見つかりませんでした".to_string());
        }
        std::fs::copy(&built, dest_file).map_err(|e| e.to_string())?;
        make_executable(dest_file)
    }

    /// Fetches upstream's own distribution: Windows gets the prebuilt
    /// `tsreadex.exe` out of the release zip, every other platform has to
    /// build the tagged source with `make` (upstream ships Windows binaries
    /// only). Used only when our own prebuilt asset can't be had.
    fn install_from_upstream(tmp_dir: &Path, dest_file: &Path) -> Result<(), String> {
        let release = fetch_latest_release_with_asset()?;
        if cfg!(target_os = "windows") {
            install_windows(&release, tmp_dir, dest_file)
        } else {
            install_unix(&release, tmp_dir, dest_file)
        }
    }

    /// Fetches tsreadex into `<install_dir>/thirdparty/tsreadex/`.
    ///
    /// Our own release asset comes first: it covers every supported platform
    /// with a prebuilt binary, so no local C++ toolchain is needed. Upstream
    /// is the fallback for when this project hasn't published a matching
    /// asset (or GitHub is unreachable for that repo) — on Unix that path
    /// compiles from source and therefore needs `make` and a C++ compiler.
    pub fn resolve_tsreadex(install_dir: &Path) -> Result<PathBuf, String> {
        let dest_dir = install_dir.join("thirdparty").join("tsreadex");
        std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
        let dest_file = dest_dir.join(exe_file_name("tsreadex"));

        let tmp_dir = std::env::temp_dir().join(format!("recisdb-proxy-tsreadex-dl-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

        let result = install_from_own_release(&tmp_dir, &dest_file).or_else(|own_err| {
            install_from_upstream(&tmp_dir, &dest_file)
                .map_err(|upstream_err| format!("{own_err} / 上流からの取得も失敗しました: {upstream_err}"))
        });
        let _ = std::fs::remove_dir_all(&tmp_dir);
        result?;
        Ok(dest_file)
    }
}

#[cfg(not(feature = "webhook"))]
mod tsreadex_setup {
    use super::*;

    pub fn resolve_tsreadex(_install_dir: &Path) -> Result<PathBuf, String> {
        Err(
            "tsreadexの自動取得には webhook フィーチャーを有効にしてビルドしてください \
             (デフォルトのビルドでは有効です)。"
                .to_string(),
        )
    }
}

/// Detect an existing tsreadex, else fetch/build one. Unlike ffmpeg this is
/// allowed to fail: the caller ([`ensure_preview_ready`]) treats an `Err`
/// here as a warning, not a hard failure — the preview pipeline still works
/// without a preprocessor, just without per-service ID3 caption conversion.
fn resolve_tsreadex_ready(install_dir: &Path) -> Result<PathBuf, String> {
    if let Some(path) = detect_tsreadex(install_dir) {
        return Ok(path);
    }
    tsreadex_setup::resolve_tsreadex(install_dir)
}

// ============================================================================
// TOML [preview] rewrite (main.rs re-applies the TOML `[preview]` section to
// the DB on every startup, so skipping this step would make the auto-setup
// silently undo itself on the next restart).
// ============================================================================

/// Cheap existence check mirroring `setup_helpers::replace_scalar_in_section`'s
/// own per-line section tracking, used to fail with a clear message instead
/// of hitting that function's internal `assert!` when a hand-edited config
/// file's `[preview]` section doesn't have the expected keys.
fn toml_section_has_key(toml: &str, section: &str, key: &str) -> bool {
    let key_prefix = format!("{key} = ");
    let mut current_section = String::new();
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current_section = name.to_string();
            }
        }
        if current_section == section && trimmed.starts_with(&key_prefix) {
            return true;
        }
    }
    false
}

/// Rewrites `[preview] command_path`/`preprocessor_path` in the config file
/// at `config_path` to the resolved paths, leaving every other line
/// (including comments and other sections) untouched.
fn write_preview_paths_to_toml(config_path: &Path, command_path: &str, preprocessor_path: &str) -> Result<(), String> {
    let contents = std::fs::read_to_string(config_path).map_err(|e| e.to_string())?;

    if !toml_section_has_key(&contents, "preview", "command_path") || !toml_section_has_key(&contents, "preview", "preprocessor_path")
    {
        return Err(format!(
            "{} の [preview] セクションに command_path / preprocessor_path が見つかりませんでした。\
             手動で追記してください。",
            config_path.display()
        ));
    }

    // Windows paths contain backslashes, which are TOML escape characters
    // inside a basic (double-quoted) string — must be escaped the same way
    // `setup_helpers::generate_config` escapes the database path.
    let command_path = crate::setup_helpers::escape_toml_basic_string(command_path);
    let preprocessor_path = crate::setup_helpers::escape_toml_basic_string(preprocessor_path);

    let rewritten = crate::setup_helpers::replace_scalar_in_section(&contents, "preview", "command_path", &command_path);
    let rewritten = crate::setup_helpers::replace_scalar_in_section(&rewritten, "preview", "preprocessor_path", &preprocessor_path);

    std::fs::write(config_path, rewritten).map_err(|e| e.to_string())
}

// ============================================================================
// Top-level entry point
// ============================================================================

/// Updates the seeded preview encode profiles' `extra_args` to the freshly
/// selected ffmpeg template, but only if the row is still at a value this
/// codebase generated automatically (the QSVEncC seed, the pre-two-stage
/// legacy template, or a previously auto-generated ffmpeg template for any
/// encoder). An admin's own edit is left completely untouched — same rule
/// `database::encode_profile::seed_default_encode_profiles` applies when
/// migrating the legacy template.
fn update_preview_profile_args_if_default(db: &Database, video_encoder: &str) -> Result<(), String> {
    let profiles = db.get_all_encode_profiles().map_err(|e| e.to_string())?;
    let Some(profile) = profiles.iter().find(|p| p.name == "preview-h264") else {
        return Err("preview-h264 プロファイルが見つかりません".to_string());
    };

    if crate::database::preview_extra_args_is_auto_generated(profile.extra_args.as_deref()) {
        let new_args = crate::database::preview_encode_args_ffmpeg(video_encoder);
        db.update_encode_profile(profile.id, None, None, None, None, None, Some(Some(&new_args)), None)
            .map_err(|e| e.to_string())?;
    }

    if let Some(profile_4k) = profiles.iter().find(|p| p.name == "preview-4k") {
        if crate::database::preview_4k_extra_args_is_auto_generated(profile_4k.extra_args.as_deref()) {
            let new_args = crate::database::preview_4k_encode_args_ffmpeg(video_encoder);
            db.update_encode_profile(
                profile_4k.id,
                None,
                None,
                None,
                None,
                None,
                Some(Some(&new_args)),
                None,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Detects (or downloads) an encoder and a preprocessor for the browser
/// preview pipeline, writes both into the DB and — if `config_path` is
/// known and exists — into the `[preview]` section of the TOML config file,
/// then enables `preview_encoder_config`.
///
/// tsreadex (the preprocessor) is optional: failure there is folded into
/// `warnings` and preview is still enabled (with `preprocessor_path` left
/// empty). ffmpeg (the encoder) is not optional: failure there is returned
/// as `Err` and nothing is written to the DB.
pub fn ensure_preview_ready(db: &Database, install_dir: &Path, config_path: Option<&Path>) -> Result<PreviewSetupReport, String> {
    let mut warnings = Vec::new();

    let (encoder_path, encoder_source, encoders_output) = resolve_ffmpeg(install_dir)?;
    let video_encoder = select_working_video_encoder(&encoder_path, &encoders_output);

    let preprocessor_path = match resolve_tsreadex_ready(install_dir) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(e) => {
            warnings.push(format!(
                "tsreadex(前段処理)の自動セットアップに失敗しました。前段なしでプレビューを有効化します: {e}"
            ));
            String::new()
        }
    };

    let encoder_path_str = encoder_path.to_string_lossy().into_owned();

    db.set_preview_command_path(&encoder_path_str).map_err(|e| e.to_string())?;
    db.set_preview_preprocessor_path(&preprocessor_path).map_err(|e| e.to_string())?;

    if let Err(e) = update_preview_profile_args_if_default(db, &video_encoder) {
        warnings.push(format!("エンコードプロファイルの更新に失敗しました(手動で確認してください): {e}"));
    }

    db.update_preview_encoder_config(true, crate::database::DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS, 10_000)
        .map_err(|e| e.to_string())?;

    match config_path {
        Some(path) if path.exists() => {
            if let Err(e) = write_preview_paths_to_toml(path, &encoder_path_str, &preprocessor_path) {
                warnings.push(format!(
                    "{} の書き換えに失敗しました。次回起動時にこの設定が失われる可能性があります: {e}",
                    path.display()
                ));
            }
        }
        Some(path) => {
            warnings.push(format!(
                "設定ファイル {} が見つからないため [preview] セクションを更新できませんでした。\
                 次回起動時にこの設定が失われる可能性があります。",
                path.display()
            ));
        }
        None => {
            warnings.push(
                "設定ファイルのパスが不明なため [preview] セクションを更新できませんでした。\
                 次回起動時にこの設定が失われる可能性があります。"
                    .to_string(),
            );
        }
    }

    Ok(PreviewSetupReport {
        enabled: true,
        encoder_path: encoder_path_str,
        encoder_source: encoder_source.to_string(),
        video_encoder,
        preprocessor_path,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- BtbN asset name matching (pure, no network) -----------------------
    //
    // Regression guard: the previously hardcoded
    // `ffmpeg-n8.1-latest-win64-gpl.zip` 404s because release-branch assets
    // carry a version suffix (`...-gpl-8.1.zip`).

    #[cfg(feature = "webhook")]
    #[test]
    fn btbn_asset_matching_accepts_versioned_static_gpl_names() {
        use ffmpeg_download::is_static_gpl_asset as m;

        // Release-branch build, with and without the version suffix.
        assert!(m("ffmpeg-n8.1-latest-win64-gpl-8.1.zip", "ffmpeg-n8.1-latest-", "win64", ".zip"));
        assert!(m("ffmpeg-n8.1-latest-win64-gpl.zip", "ffmpeg-n8.1-latest-", "win64", ".zip"));
        assert!(m(
            "ffmpeg-n8.1-latest-linuxarm64-gpl-8.1.tar.xz",
            "ffmpeg-n8.1-latest-",
            "linuxarm64",
            ".tar.xz"
        ));
        // master build (the offline fallback's shape).
        assert!(m("ffmpeg-master-latest-win64-gpl.zip", "ffmpeg-master-latest-", "win64", ".zip"));

        // Shared builds need an accompanying lib/ dir -> must be rejected.
        assert!(!m("ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip", "ffmpeg-n8.1-latest-", "win64", ".zip"));
        assert!(!m("ffmpeg-master-latest-win64-gpl-shared.zip", "ffmpeg-master-latest-", "win64", ".zip"));
        // LGPL builds lack libx264, which verify_ffmpeg_binary requires.
        assert!(!m("ffmpeg-n8.1-latest-win64-lgpl-8.1.zip", "ffmpeg-n8.1-latest-", "win64", ".zip"));
        // Wrong platform / wrong extension.
        assert!(!m("ffmpeg-n8.1-latest-winarm64-gpl-8.1.zip", "ffmpeg-n8.1-latest-", "win64", ".zip"));
        assert!(!m("ffmpeg-n8.1-latest-linux64-gpl-8.1.tar.xz", "ffmpeg-n8.1-latest-", "linux64", ".zip"));
    }

    // -- tsreadex asset naming (pure, no network) --------------------------
    //
    // These suffixes must match the artifact names produced by the matrix in
    // `.github/workflows/tsreadex.yml`; a mismatch silently degrades every
    // platform to the upstream fallback (source + `make` on Unix).

    #[cfg(feature = "webhook")]
    #[test]
    fn tsreadex_asset_suffix_covers_every_released_platform() {
        use tsreadex_setup::own_asset_suffix as s;

        assert_eq!(s("windows", "x86_64").as_deref(), Some("-win-x64.zip"));
        assert_eq!(s("windows", "x86").as_deref(), Some("-win-x86.zip"));
        assert_eq!(s("windows", "aarch64").as_deref(), Some("-win-arm64.zip"));
        assert_eq!(s("linux", "x86_64").as_deref(), Some("-linux-amd64.tar.gz"));
        assert_eq!(s("linux", "aarch64").as_deref(), Some("-linux-arm64.tar.gz"));
        assert_eq!(s("macos", "x86_64").as_deref(), Some("-macos-amd64.tar.gz"));
        assert_eq!(s("macos", "aarch64").as_deref(), Some("-macos-arm64.tar.gz"));

        // Unreleased platforms must fall through to the upstream path
        // instead of fetching a wrong-architecture binary.
        assert_eq!(s("linux", "arm"), None);
        assert_eq!(s("freebsd", "x86_64"), None);

        // The host this binary runs on must resolve to *some* asset — the
        // whole point is that no platform needs a local C++ toolchain.
        assert!(s(std::env::consts::OS, std::env::consts::ARCH).is_some());
    }

    // -- hardware encoder selection (pure, no ffmpeg binary needed) --------

    #[test]
    fn macos_prefers_videotoolbox_when_listed() {
        let listing = " V..... h264_videotoolbox    VideoToolbox H.264 Encoder\n V..... libx264              libx264 H.264\n";
        assert_eq!(listed_hardware_encoders(HostOs::MacOs, listing), vec!["h264_videotoolbox"]);
    }

    #[test]
    fn macos_lists_nothing_when_videotoolbox_absent() {
        let listing = " V..... libx264              libx264 H.264\n";
        assert!(listed_hardware_encoders(HostOs::MacOs, listing).is_empty());
        // No candidates listed -> select_working_video_encoder must fall
        // back to libx264 without touching the (nonexistent) ffmpeg path.
        assert_eq!(
            select_working_video_encoder(Path::new("/nonexistent/ffmpeg"), listing),
            "libx264"
        );
    }

    #[test]
    fn windows_prefers_qsv_then_nvenc_then_amf() {
        let listing = "h264_amf\nh264_nvenc\nh264_qsv\nlibx264\n";
        assert_eq!(
            listed_hardware_encoders(HostOs::Windows, listing),
            vec!["h264_qsv", "h264_nvenc", "h264_amf"]
        );
    }

    #[test]
    fn windows_falls_back_to_whatever_subset_is_listed() {
        let listing = "h264_amf\nlibx264\n";
        assert_eq!(listed_hardware_encoders(HostOs::Windows, listing), vec!["h264_amf"]);
    }

    #[test]
    fn linux_prefers_qsv_then_nvenc_then_vaapi() {
        let listing = "h264_vaapi\nh264_qsv\nh264_nvenc\nlibx264\n";
        assert_eq!(
            listed_hardware_encoders(HostOs::Linux, listing),
            vec!["h264_qsv", "h264_nvenc", "h264_vaapi"]
        );
    }

    #[test]
    fn no_hardware_candidates_listed_anywhere_means_libx264_fallback() {
        let listing = "libx264\nmpeg4\n";
        for os in [HostOs::MacOs, HostOs::Windows, HostOs::Linux] {
            assert!(listed_hardware_encoders(os, listing).is_empty(), "{os:?} should have no candidates");
        }
        assert_eq!(select_working_video_encoder(Path::new("/nonexistent/ffmpeg"), listing), "libx264");
    }

    // -- ffmpeg argument template --------------------------------------

    #[test]
    fn ffmpeg_args_target_stdin_stdout_mpegts_with_requested_encoder() {
        let args = crate::database::preview_encode_args_ffmpeg("h264_qsv");
        assert!(args.contains("-f mpegts -i pipe:0"), "must read mpegts from stdin: {args}");
        assert!(args.contains("-c:v h264_qsv"), "must use the requested encoder: {args}");
        assert!(args.contains("pipe:1"), "must write to stdout: {args}");
        assert!(args.contains("-map 0:d?"), "must pass through the ID3 timed-metadata stream: {args}");
        // x264 固有のオプションがハードウェアエンコーダに漏れていないこと。
        // (`-preset veryfast` は QSV にも同名のプリセットがあるので、判定材料に
        // 使えるのは x264 にしかない `-tune zerolatency` の方。)
        assert!(!args.contains("zerolatency"), "libx264 専用のオプションが漏れている: {args}");
    }

    #[test]
    fn every_encoder_gets_its_own_tuning_embedded_verbatim() {
        // 本番の引数に埋まる調整オプションと、候補選定のテストエンコードで
        // 使うものが同一であること。ここがずれると「選定は通ったのに視聴開始で
        // 落ちる」状態になる。
        for encoder in ["libx264", "h264_videotoolbox", "h264_qsv", "h264_nvenc", "h264_amf"] {
            let tuning = crate::database::video_encoder_tuning(encoder);
            assert!(!tuning.is_empty(), "{encoder} の調整オプションが空");
            let args = crate::database::preview_encode_args_ffmpeg(encoder);
            assert!(
                args.contains(&format!("-c:v {encoder} {tuning}")),
                "{encoder}: 調整オプションがエンコーダ指定の直後に入っていない: {args}"
            );
        }
    }

    #[test]
    fn libx264_is_tuned_for_low_latency() {
        let args = crate::database::preview_encode_args_ffmpeg("libx264");
        assert!(args.contains("-c:v libx264 -preset veryfast -tune zerolatency"), "{args}");
    }

    #[test]
    fn vaapi_has_no_tuning_so_it_falls_back_instead_of_half_working() {
        // VAAPI は -vaapi_device と hwupload が要り、このテンプレートの形では
        // 動かせない。中途半端なオプションを付けず、テストエンコードで落として
        // libx264 に回す方針。
        assert_eq!(crate::database::video_encoder_tuning("h264_vaapi"), "");
    }

    // -- TOML [preview] rewrite ------------------------------------------

    fn unique_temp_path(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("recisdb-proxy-preview-setup-test-{label}-{n}-{}.toml", std::process::id()))
    }

    const SAMPLE_TOML: &str = r#"# sample config
[server]
listen = "0.0.0.0:1234"

[preview]
# comment kept as-is
command_path = "C:\\DTV\\KonomiTV\\server\\thirdparty\\QSVEncC\\QSVEncC.exe"
preprocessor_path = "C:\\DTV\\KonomiTV\\server\\thirdparty\\tsreadex\\tsreadex.exe"

[tls]
enabled = false
"#;

    #[test]
    fn write_preview_paths_rewrites_both_keys_and_nothing_else() {
        let path = unique_temp_path("rewrite-both");
        std::fs::write(&path, SAMPLE_TOML).unwrap();

        write_preview_paths_to_toml(&path, "/opt/recisdb-proxy/thirdparty/ffmpeg/ffmpeg", "/opt/recisdb-proxy/thirdparty/tsreadex/tsreadex").unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains(r#"command_path = "/opt/recisdb-proxy/thirdparty/ffmpeg/ffmpeg""#), "{updated}");
        assert!(
            updated.contains(r#"preprocessor_path = "/opt/recisdb-proxy/thirdparty/tsreadex/tsreadex""#),
            "{updated}"
        );
        // Untouched sections/lines.
        assert!(updated.contains(r#"listen = "0.0.0.0:1234""#));
        assert!(updated.contains("enabled = false"));
        assert!(updated.contains("# comment kept as-is"));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_preview_paths_escapes_windows_backslashes() {
        let path = unique_temp_path("rewrite-escape");
        std::fs::write(&path, SAMPLE_TOML).unwrap();

        write_preview_paths_to_toml(&path, r"C:\recisdb-proxy\thirdparty\ffmpeg\ffmpeg.exe", "").unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(
            updated.contains(r#"command_path = "C:\\recisdb-proxy\\thirdparty\\ffmpeg\\ffmpeg.exe""#),
            "{updated}"
        );
        assert!(updated.contains(r#"preprocessor_path = """#), "empty preprocessor path should be written verbatim: {updated}");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_preview_paths_fails_clearly_when_keys_are_missing() {
        let path = unique_temp_path("rewrite-missing-key");
        std::fs::write(&path, "[preview]\n# no keys here\n").unwrap();

        let err = write_preview_paths_to_toml(&path, "/x/ffmpeg", "/x/tsreadex").unwrap_err();
        assert!(err.contains("command_path"), "{err}");

        std::fs::remove_file(&path).unwrap();
    }

    // -- "is this still the default extra_args" gate ----------------------

    #[test]
    fn default_profile_extra_args_get_replaced_but_user_edits_do_not() {
        use crate::database::Database;

        let db = Database::open_in_memory().unwrap();

        // Freshly seeded row (QSVEncC default) must count as replaceable.
        let seeded = db.get_all_encode_profiles().unwrap().into_iter().find(|p| p.name == "preview-h264").unwrap();
        assert!(crate::database::preview_extra_args_is_auto_generated(seeded.extra_args.as_deref()));

        update_preview_profile_args_if_default(&db, "h264_videotoolbox").unwrap();
        let updated = db.get_all_encode_profiles().unwrap().into_iter().find(|p| p.name == "preview-h264").unwrap();
        assert_eq!(
            updated.extra_args.as_deref(),
            Some(crate::database::preview_encode_args_ffmpeg("h264_videotoolbox").as_str())
        );
        let updated_4k = db.get_all_encode_profiles().unwrap().into_iter().find(|p| p.name == "preview-4k").unwrap();
        assert_eq!(
            updated_4k.extra_args.as_deref(),
            Some(crate::database::preview_4k_encode_args_ffmpeg("h264_videotoolbox").as_str())
        );

        // Re-running with a different encoder must still replace it (this
        // is still a value the tool itself generated, not an admin edit).
        update_preview_profile_args_if_default(&db, "h264_qsv").unwrap();
        let updated = db.get_all_encode_profiles().unwrap().into_iter().find(|p| p.name == "preview-h264").unwrap();
        assert_eq!(
            updated.extra_args.as_deref(),
            Some(crate::database::preview_encode_args_ffmpeg("h264_qsv").as_str())
        );

        // An admin's own edit must never be overwritten.
        db.update_encode_profile(updated.id, None, None, None, None, None, Some(Some("--my-custom-args")), None)
            .unwrap();
        db.update_encode_profile(updated_4k.id, None, None, None, None, None, Some(Some("--my-custom-4k-args")), None)
            .unwrap();
        update_preview_profile_args_if_default(&db, "libx264").unwrap();
        let profiles = db.get_all_encode_profiles().unwrap();
        let after = profiles.iter().find(|p| p.name == "preview-h264").unwrap();
        assert_eq!(after.extra_args.as_deref(), Some("--my-custom-args"));
        let after_4k = profiles.iter().find(|p| p.name == "preview-4k").unwrap();
        assert_eq!(after_4k.extra_args.as_deref(), Some("--my-custom-4k-args"));
    }
}
