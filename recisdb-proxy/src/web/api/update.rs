//! Update-notification and self-update endpoints.
//!
//! `GET /api/version` (in `statics.rs`) reports only this server's own
//! version. This module adds the pieces that talk to GitHub:
//!
//! - `GET /api/update/check` — fetches `stuayu/recisdb-proxy-rs` releases
//!   (6h in-memory cache), returns the newest applicable stable/prerelease.
//! - `POST /api/update/apply` — downloads a specific release's platform
//!   asset, extracts the binary, validates it, and replaces the running
//!   executable in place (`self-replace` crate) before re-executing itself.
//! - `GET /api/update/status` — progress of an in-flight (or last) apply.
//!
//! The web-ui (`App.vue`) used to hit GitHub directly from the browser; that
//! is being replaced with calls to this module so the dashboard works
//! without the browser reaching the public internet, and so self-update can
//! exist at all (a browser can't replace a server-side binary).
//!
//! Everything that doesn't require the network — version comparison,
//! stable/prerelease selection, per-platform asset naming, binary
//! validation — is implemented as plain functions with no I/O so they can be
//! unit tested without a GitHub round trip. See the `tests` module below.

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::web::state::{UpdateStatus, WebState};

use super::error::ApiError;

/// How long a successful GitHub releases fetch is trusted before the next
/// `GET /api/update/check` (without `?force=true`) triggers a re-fetch.
/// GitHub's unauthenticated REST API allows 60 requests/hour/IP; several
/// dashboard tabs polling this endpoint must not burn through that budget.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

const RELEASES_URL: &str = "https://api.github.com/repos/stuayu/recisdb-proxy-rs/releases?per_page=15";

// ============================================================================
// GitHub API response shape (only the fields this module needs)
// ============================================================================

/// A single release as returned by GitHub's `GET /repos/:owner/:repo/releases`.
///
/// Deliberately public: [`select_updates`] takes a slice of these and is a
/// pure function, so tests build [`GithubRelease`] values by hand instead of
/// mocking HTTP.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    /// ISO-8601, e.g. `"2026-07-01T12:00:00Z"`. `null` for unpublished
    /// drafts, hence `Option`.
    pub published_at: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A release summarized for the `/api/update/check` response.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub url: String,
    pub published_at: Option<String>,
}

impl From<&GithubRelease> for ReleaseInfo {
    fn from(r: &GithubRelease) -> Self {
        ReleaseInfo { tag: r.tag_name.clone(), url: r.html_url.clone(), published_at: r.published_at.clone() }
    }
}

// ============================================================================
// Pure version comparison / selection logic (no I/O — unit tested below)
// ============================================================================

/// Parses a release tag into `(major, minor, patch, has_suffix)`.
///
/// - A leading `v` is stripped (`v1.2.3` and `1.2.3` compare equal).
/// - Only the portion before the first `-` is treated as the numeric
///   `major.minor.patch`; a missing `minor`/`patch` defaults to `0`
///   (`"1"` == `"1.0.0"`).
/// - `has_suffix` is true whenever a `-...` suffix is present (e.g.
///   `-beta.1`), meaning the tag is a prerelease/build tag.
///
/// Returns `None` if the numeric portion doesn't parse as up to three
/// dot-separated non-negative integers.
fn parse_version(tag: &str) -> Option<(u64, u64, u64, bool)> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    let mut halves = stripped.splitn(2, '-');
    let numeric_part = halves.next()?;
    let has_suffix = halves.next().is_some();

    let mut segments = numeric_part.split('.');
    let major = segments.next()?.parse().ok()?;
    let minor = segments.next().map(|s| s.parse().ok()).unwrap_or(Some(0))?;
    let patch = segments.next().map(|s| s.parse().ok()).unwrap_or(Some(0))?;
    // A fourth numeric segment (or anything non-numeric that slipped past
    // the '-' split) is not a version scheme we support.
    if segments.next().is_some() {
        return None;
    }

    Some((major, minor, patch, has_suffix))
}

/// Sortable key derived from [`parse_version`]: same numeric version, a
/// stable tag (`has_suffix == false`) always ranks above a prerelease tag
/// (`has_suffix == true`) — tuple/`bool` `Ord` does this for free since
/// `false < true`, so we store `!has_suffix` as the last field.
fn version_key(tag: &str) -> Option<(u64, u64, u64, bool)> {
    let (major, minor, patch, has_suffix) = parse_version(tag)?;
    Some((major, minor, patch, !has_suffix))
}

/// Picks the newest applicable stable and prerelease releases out of a raw
/// GitHub releases listing, relative to `current_version`.
///
/// Rules (see the task spec this implements):
/// - Draft releases are excluded entirely.
/// - `stable`: the newest release with GitHub's `prerelease` flag false,
///   whose parsed version is strictly newer than `current_version`. `None`
///   if there is none.
/// - `prerelease`: the newest release with `prerelease` true, strictly newer
///   than `current_version`, **and** strictly newer than `stable` (a
///   prerelease that has already been superseded by a stable release is
///   never surfaced). `None` if there is none.
/// - Releases whose tag doesn't parse as a version are ignored rather than
///   treated as errors, so one malformed tag can't take down the whole
///   check.
pub fn select_updates(current_version: &str, releases: &[GithubRelease]) -> (Option<ReleaseInfo>, Option<ReleaseInfo>) {
    // An unparsable "current" version (should not happen for our own crate
    // version, but keep this total) is treated as the lowest possible
    // version so every valid release counts as an update.
    let current = version_key(current_version).unwrap_or((0, 0, 0, false));

    let live = releases.iter().filter(|r| !r.draft);

    let stable = live
        .clone()
        .filter(|r| !r.prerelease)
        .filter_map(|r| version_key(&r.tag_name).map(|k| (k, r)))
        .filter(|(k, _)| *k > current)
        .max_by_key(|(k, _)| *k)
        .map(|(_, r)| ReleaseInfo::from(r));

    let stable_key = stable.as_ref().and_then(|s| version_key(&s.tag));

    let prerelease = live
        .filter(|r| r.prerelease)
        .filter_map(|r| version_key(&r.tag_name).map(|k| (k, r)))
        .filter(|(k, _)| *k > current)
        .filter(|(k, _)| stable_key.map(|sk| *k > sk).unwrap_or(true))
        .max_by_key(|(k, _)| *k)
        .map(|(_, r)| ReleaseInfo::from(r));

    (stable, prerelease)
}

// ============================================================================
// Self-update platform support / asset naming (pure — unit tested below)
// ============================================================================

/// Whether the given `(os, arch)` pair (as reported by `std::env::consts::OS`
/// / `std::env::consts::ARCH`) has a self-update-capable release asset.
/// Kept as a function of explicit strings (rather than `#[cfg(...)]`) so it
/// compiles and is testable identically on every host.
fn platform_supports_self_update(os: &str, arch: &str) -> bool {
    match os {
        "linux" => matches!(arch, "x86_64" | "aarch64"),
        "windows" => matches!(arch, "x86_64" | "x86"),
        "macos" => matches!(arch, "x86_64" | "aarch64"),
        _ => false,
    }
}

/// `true` on the platforms this binary was actually compiled for. Backed by
/// [`platform_supports_self_update`], fed with the compile-time constants
/// `std::env::consts::OS`/`ARCH` (which already reflect the build target,
/// not the host running `cargo test`).
fn self_update_supported() -> bool {
    platform_supports_self_update(std::env::consts::OS, std::env::consts::ARCH)
}

/// Release-CI asset filename for `(tag, os, arch)` — must match
/// `.github/workflows/release.yml`'s naming exactly:
/// - `recisdb-proxy-{tag}-linux-amd64.tar.gz` / `-linux-arm64.tar.gz`
/// - `recisdb-proxy-{tag}-macos-amd64.tar.gz` / `-macos-arm64.tar.gz`
/// - `recisdb-{tag}-win-x64.zip` / `-win-x86.zip`
///
/// `None` for any platform without a self-update asset.
fn asset_filename(tag: &str, os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("linux", "x86_64") => Some(format!("recisdb-proxy-{tag}-linux-amd64.tar.gz")),
        ("linux", "aarch64") => Some(format!("recisdb-proxy-{tag}-linux-arm64.tar.gz")),
        ("macos", "x86_64") => Some(format!("recisdb-proxy-{tag}-macos-amd64.tar.gz")),
        ("macos", "aarch64") => Some(format!("recisdb-proxy-{tag}-macos-arm64.tar.gz")),
        ("windows", "x86_64") => Some(format!("recisdb-{tag}-win-x64.zip")),
        ("windows", "x86") => Some(format!("recisdb-{tag}-win-x86.zip")),
        _ => None,
    }
}

/// [`asset_filename`] for the platform this binary was actually built for.
fn current_platform_asset_filename(tag: &str) -> Option<String> {
    asset_filename(tag, std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether an archive entry name (a tar path or a zip entry name) is the
/// executable we want to extract, for the given `os` ("windows" wants a
/// `.exe" suffix; everything else — i.e. our Linux release archives — does
/// not).
fn is_target_binary_entry(entry_name: &str, os: &str) -> bool {
    let normalized = entry_name.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or(&normalized);
    if os == "windows" {
        base == "recisdb-proxy.exe"
    } else {
        base == "recisdb-proxy"
    }
}

/// Minimum plausible size for a real `recisdb-proxy` binary. Guards against
/// a truncated download or an HTML error page being mistaken for the
/// executable.
const MIN_BINARY_SIZE: u64 = 1_000_000;

/// Checks the leading bytes of an extracted binary against the expected
/// magic for `os`: `MZ` for Windows PE, `\x7fELF` for Linux ELF, Mach-O for
/// macOS.
///
/// macOS accepts three magics because the release CI could plausibly ship
/// any of them: 64-bit Mach-O in either endianness (`feedfacf`), and the
/// universal/"fat" wrapper (`cafebabe`, always big-endian) if the two
/// per-arch builds are ever `lipo`-merged into one asset. 32-bit Mach-O
/// (`feedface`) is not accepted — no supported target produces it.
fn has_valid_magic(magic: &[u8], os: &str) -> bool {
    match os {
        "windows" => magic.starts_with(b"MZ"),
        "macos" => {
            magic.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])  // Mach-O 64, LE
                || magic.starts_with(&[0xfe, 0xed, 0xfa, 0xcf]) // Mach-O 64, BE
                || magic.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) // universal
        }
        _ => magic.starts_with(b"\x7fELF"),
    }
}

// ============================================================================
// GET /api/update/check
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CheckQuery {
    #[serde(default)]
    force: bool,
}

/// `GET /api/update/check` — see module docs.
pub async fn check_update(State(web_state): State<Arc<WebState>>, Query(query): Query<CheckQuery>) -> Json<serde_json::Value> {
    let current_version = crate::VERSION;

    let releases = fetch_releases_cached(&web_state, query.force).await;
    let (stable, prerelease) = select_updates(current_version, &releases);

    Json(json!({
        "current_version": current_version,
        "stable": stable,
        "prerelease": prerelease,
        "self_update_supported": self_update_supported(),
    }))
}

/// Returns the cached releases listing if it's fresh (or `force` is false
/// and a cache exists at all being stale is still preferred over hammering
/// GitHub — see the "not force" branch below), otherwise fetches a new one.
///
/// On fetch failure this logs a warning and returns an empty `Vec` **without
/// touching the cache** — a transient GitHub outage must not poison the
/// cache with "no releases" for the next 6 hours; the next non-force check
/// will simply retry since the (still-stale) cache stays stale.
async fn fetch_releases_cached(web_state: &WebState, force: bool) -> Vec<GithubRelease> {
    if !force {
        let cache = web_state.update_check_cache.read().await;
        if let Some(cached) = cache.as_ref() {
            if cached.fetched_at.elapsed() < CACHE_TTL {
                return cached.releases.clone();
            }
        }
    }

    match fetch_releases_from_github().await {
        Ok(releases) => {
            *web_state.update_check_cache.write().await =
                Some(crate::web::state::UpdateCheckCache { fetched_at: Instant::now(), releases: releases.clone() });
            releases
        }
        Err(e) => {
            tracing::warn!("update check: failed to reach GitHub releases API: {e}");
            Vec::new()
        }
    }
}

async fn fetch_releases_from_github() -> Result<Vec<GithubRelease>, String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("recisdb-proxy/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API returned HTTP {}", resp.status()));
    }

    resp.json::<Vec<GithubRelease>>().await.map_err(|e| e.to_string())
}

// ============================================================================
// GET /api/update/status
// ============================================================================

/// `GET /api/update/status` — see module docs.
pub async fn update_status(State(web_state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let status = web_state.update_status.lock().await.clone();
    Json(status_json(&status))
}

fn status_json(status: &UpdateStatus) -> serde_json::Value {
    let (state, message) = match status {
        UpdateStatus::Idle => ("idle", None),
        UpdateStatus::Downloading => ("downloading", None),
        UpdateStatus::Extracting => ("extracting", None),
        UpdateStatus::Replacing => ("replacing", None),
        UpdateStatus::Restarting => ("restarting", None),
        UpdateStatus::Error(message) => ("error", Some(message.clone())),
    };
    json!({ "state": state, "message": message })
}

// ============================================================================
// POST /api/update/apply
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ApplyUpdateRequest {
    pub tag: String,
}

/// `POST /api/update/apply` — see module docs.
///
/// Only ever touches the *running* executable at the very last step
/// (`self_replace::self_replace`, inside `run_self_update`); every step
/// before that (download, extract, validate) operates on temporary files
/// next to the executable, so any failure prior to that call leaves the
/// current binary completely untouched.
pub async fn apply_update(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<ApplyUpdateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !self_update_supported() {
        return Err(ApiError::not_implemented(format!(
            "self-update is not supported on this build ({}/{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )));
    }

    let releases = fetch_releases_cached(&web_state, false).await;
    let release = releases
        .iter()
        .find(|r| r.tag_name == payload.tag)
        .ok_or_else(|| ApiError::not_found(format!("release '{}' not found", payload.tag)))?;

    let filename = current_platform_asset_filename(&payload.tag)
        .ok_or_else(|| ApiError::internal("no release asset name for this platform"))?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == filename)
        .ok_or_else(|| ApiError::not_found(format!("release '{}' has no asset named '{filename}'", payload.tag)))?;
    let download_url = asset.browser_download_url.clone();
    let tag = payload.tag.clone();

    // Claim the "busy" slot atomically: only Idle/Error may start a new run.
    {
        let mut status = web_state.update_status.lock().await;
        if !matches!(*status, UpdateStatus::Idle | UpdateStatus::Error(_)) {
            return Err(ApiError::conflict("a self-update is already in progress"));
        }
        *status = UpdateStatus::Downloading;
    }

    tokio::spawn(run_self_update(Arc::clone(&web_state), tag, download_url));

    Ok(Json(json!({
        "success": true,
        "message": "update started",
    })))
}

async fn set_status(web_state: &WebState, status: UpdateStatus) {
    *web_state.update_status.lock().await = status;
}

/// Background task spawned by `apply_update`. Never panics on failure paths
/// — every fallible step is `Result`-based and funnelled into
/// `UpdateStatus::Error` so a bad release/network hiccup can't take the
/// whole server down.
async fn run_self_update(web_state: Arc<WebState>, tag: String, download_url: String) {
    if let Err(message) = run_self_update_inner(&web_state, &tag, &download_url).await {
        tracing::warn!("self-update ({tag}) failed: {message}");
        set_status(&web_state, UpdateStatus::Error(message)).await;
    }
    // On success `run_self_update_inner` never returns (Unix `exec`) or the
    // process has already called `std::process::exit` (Windows), so
    // reaching here always means failure.
}

async fn run_self_update_inner(web_state: &WebState, tag: &str, download_url: &str) -> Result<(), String> {
    tracing::info!("self-update: starting update to {tag}");
    let os = std::env::consts::OS;
    let exe_path = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe_dir = exe_path.parent().ok_or("current executable has no parent directory")?.to_path_buf();
    let pid = std::process::id();
    let archive_path = exe_dir.join(format!(".recisdb-proxy-update-{pid}.download"));
    let extracted_path = exe_dir.join(format!(".recisdb-proxy-update-{pid}.new"));

    // --- Download (streamed to disk; the archive is never fully buffered
    // in memory) -------------------------------------------------------
    set_status(web_state, UpdateStatus::Downloading).await;
    download_to_file(download_url, &archive_path).await.map_err(|e| {
        let _ = std::fs::remove_file(&archive_path);
        format!("download failed: {e}")
    })?;

    // --- Extract (blocking file/archive I/O off the async runtime) -----
    set_status(web_state, UpdateStatus::Extracting).await;
    let extract_result = {
        let archive_path = archive_path.clone();
        let extracted_path = extracted_path.clone();
        let os = os.to_string();
        tokio::task::spawn_blocking(move || extract_archive(&archive_path, &extracted_path, &os))
            .await
            .map_err(|e| format!("extract task panicked: {e}"))?
    };
    let _ = tokio::fs::remove_file(&archive_path).await;
    extract_result.map_err(|e| {
        let _ = std::fs::remove_file(&extracted_path);
        format!("extraction failed: {e}")
    })?;

    // --- Validate before touching the running binary --------------------
    if let Err(e) = validate_extracted_binary(&extracted_path, os).await {
        let _ = tokio::fs::remove_file(&extracted_path).await;
        return Err(e);
    }

    // --- Replace the running binary -------------------------------------
    set_status(web_state, UpdateStatus::Replacing).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&extracted_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }
    {
        let extracted_path = extracted_path.clone();
        tokio::task::spawn_blocking(move || self_replace::self_replace(&extracted_path))
            .await
            .map_err(|e| format!("self_replace task panicked: {e}"))?
            .map_err(|e| format!("self_replace failed: {e}"))?;
    }
    // Best-effort: self_replace copies `extracted_path`'s contents into
    // place, it does not consume the source file (see its doc comment).
    let _ = tokio::fs::remove_file(&extracted_path).await;

    // --- Restart ----------------------------------------------------------
    set_status(web_state, UpdateStatus::Restarting).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // サービス配下 (systemd/launchd/SCM) かどうかで再起動方法が変わる
    // ため、`service::restart_self` に一本化している。
    let _ = exe_path;
    crate::service::restart_self().map_err(|e| e.to_string())
}

async fn download_to_file(url: &str, dest: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("recisdb-proxy/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;

    let mut resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Extracts just the `recisdb-proxy`/`recisdb-proxy.exe` entry out of the
/// downloaded archive into `out_path`. Blocking I/O — always called via
/// `spawn_blocking`.
fn extract_archive(archive_path: &Path, out_path: &Path, os: &str) -> Result<(), String> {
    if os == "windows" {
        let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            if is_target_binary_entry(entry.name(), os) {
                let mut out = std::fs::File::create(out_path).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
        Err("archive does not contain recisdb-proxy.exe".to_string())
    } else {
        let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        let entries = archive.entries().map_err(|e| e.to_string())?;
        for entry in entries {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
            if is_target_binary_entry(&path, os) {
                let mut out = std::fs::File::create(out_path).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
        Err("archive does not contain a recisdb-proxy binary".to_string())
    }
}

async fn validate_extracted_binary(path: &Path, os: &str) -> Result<(), String> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?;
    if meta.len() <= MIN_BINARY_SIZE {
        return Err(format!("downloaded binary is implausibly small ({} bytes)", meta.len()));
    }

    use tokio::io::AsyncReadExt;
    let mut magic = [0u8; 4];
    let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    file.read_exact(&mut magic).await.map_err(|e| format!("failed to read header: {e}"))?;

    if !has_valid_magic(&magic, os) {
        return Err("downloaded binary failed the magic-byte signature check".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, draft: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_string(),
            draft,
            prerelease,
            published_at: Some("2026-01-01T00:00:00Z".to_string()),
            html_url: format!("https://github.com/stuayu/recisdb-proxy-rs/releases/tag/{tag}"),
            assets: vec![],
        }
    }

    // -- parse_version / version_key ---------------------------------------

    #[test]
    fn parse_version_handles_v_prefix_and_missing_segments() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3, false)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3, false)));
        assert_eq!(parse_version("v1"), Some((1, 0, 0, false)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0, false)));
    }

    #[test]
    fn parse_version_detects_suffix() {
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3, true)));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3, true)));
    }

    #[test]
    fn parse_version_rejects_garbage() {
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn parse_version_handles_git_describe_dev_suffix() {
        // `crate::VERSION` on a commit after a tag looks like
        // `0.0.1-alpha.6-1-g05a127c` (git describe --tags --always --dirty,
        // leading `v` already stripped by build.rs). Only the numeric
        // portion before the *first* '-' is significant, so this parses
        // exactly like the tag it's built on top of.
        assert_eq!(parse_version("0.0.1-alpha.6-1-g05a127c"), Some((0, 0, 1, true)));
        assert_eq!(parse_version("v0.0.1-alpha.6-1-g05a127c-dirty"), Some((0, 0, 1, true)));
    }

    #[test]
    fn parse_version_rejects_bare_commit_hash() {
        // git describe --always falls back to a bare abbreviated commit hash
        // when the repo has no tags at all reachable from HEAD.
        assert_eq!(parse_version("05a127c"), None);
    }

    #[test]
    fn version_key_ranks_stable_above_prerelease_at_same_numeric_version() {
        let stable = version_key("1.2.3").unwrap();
        let pre = version_key("1.2.3-beta.1").unwrap();
        assert!(stable > pre, "stable {stable:?} should outrank prerelease {pre:?}");
    }

    // -- select_updates ------------------------------------------------------

    #[test]
    fn select_updates_finds_newer_stable() {
        let releases = vec![release("v0.1.0", false, false), release("v0.2.0", false, false)];
        let (stable, prerelease) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_returns_none_when_current_is_latest() {
        let releases = vec![release("v0.1.0", false, false), release("v0.2.0", false, false)];
        let (stable, prerelease) = select_updates("0.2.0", &releases);
        assert!(stable.is_none());
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_returns_none_when_current_is_newer_than_all_releases() {
        let releases = vec![release("v0.1.0", false, false)];
        let (stable, prerelease) = select_updates("9.9.9", &releases);
        assert!(stable.is_none());
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_surfaces_prerelease_only_release() {
        let releases = vec![release("v0.2.0-beta.1", true, false)];
        let (stable, prerelease) = select_updates("0.1.0", &releases);
        assert!(stable.is_none());
        assert_eq!(prerelease.unwrap().tag, "v0.2.0-beta.1");
    }

    #[test]
    fn select_updates_hides_prerelease_superseded_by_stable() {
        // A 0.2.0-beta.1 prerelease exists, but 0.2.0 stable has since
        // shipped — the prerelease must not be surfaced anymore.
        let releases = vec![release("v0.2.0-beta.1", true, false), release("v0.2.0", false, false)];
        let (stable, prerelease) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
        assert!(prerelease.is_none(), "prerelease behind stable must not be surfaced");
    }

    #[test]
    fn select_updates_surfaces_prerelease_newer_than_stable() {
        // Stable 0.2.0 shipped, but there's already a newer 0.3.0-beta.1.
        let releases = vec![release("v0.2.0", false, false), release("v0.3.0-beta.1", true, false)];
        let (stable, prerelease) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
        assert_eq!(prerelease.unwrap().tag, "v0.3.0-beta.1");
    }

    #[test]
    fn select_updates_excludes_drafts() {
        let releases = vec![release("v0.9.0", false, true) /* draft */, release("v0.2.0", false, false)];
        let (stable, _) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0", "draft must never be selected");
    }

    #[test]
    fn select_updates_ignores_unparsable_tags_without_erroring() {
        let releases = vec![release("not-a-version", false, false), release("v0.2.0", false, false)];
        let (stable, _) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
    }

    #[test]
    fn select_updates_picks_max_across_multiple_newer_releases() {
        let releases =
            vec![release("v0.2.0", false, false), release("v0.4.0", false, false), release("v0.3.0", false, false)];
        let (stable, _) = select_updates("0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "v0.4.0", "must pick the highest, not the first newer one");
    }

    #[test]
    fn select_updates_works_without_v_prefix_on_either_side() {
        let releases = vec![release("0.2.0", false, false)];
        let (stable, _) = select_updates("v0.1.0", &releases);
        assert_eq!(stable.unwrap().tag, "0.2.0");
    }

    // -- select_updates with git-describe-shaped current_version -------------
    //
    // `crate::VERSION` (used as `current_version` in production) is not
    // always a clean tag: on a commit after a tag it's
    // `0.0.1-alpha.6-1-g05a127c`, and on an untagged checkout it can be a
    // bare commit hash. Neither must panic or misbehave.

    #[test]
    fn select_updates_dev_build_between_tags_still_finds_newer_stable() {
        // Built from a commit 1 past v0.1.0; v0.2.0 has since shipped and
        // must still be surfaced as an update.
        let releases = vec![release("v0.1.0", false, false), release("v0.2.0", false, false)];
        let (stable, prerelease) = select_updates("0.1.0-1-g05a127c", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_dev_build_at_latest_tag_reports_no_update() {
        // Built exactly at the newest tag (clean tree): `git describe`
        // yields just the tag itself (no `-N-g<hash>` suffix, since N=0) —
        // must not claim that same tag is a pending update.
        let releases = vec![release("v0.2.0", false, false)];
        let (stable, prerelease) = select_updates("0.2.0", &releases);
        assert!(stable.is_none());
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_dirty_build_at_latest_tag_is_a_known_false_positive() {
        // `git describe --dirty` appends `-dirty` even with zero commits
        // past the tag, which `parse_version` treats identically to a
        // `-alpha.N` prerelease suffix (`has_suffix = true`). That makes a
        // same-version *stable* release outrank a same-version *dirty* dev
        // build (see `version_key`'s doc comment: stable always ranks above
        // any suffixed tag at equal major.minor.patch). Net effect: a local
        // dev build with uncommitted changes, sitting exactly on the latest
        // tag, spuriously reports that same tag as "available". This is a
        // pre-existing property of `parse_version`/`version_key` (already
        // true for e.g. "1.2.3-custom" today), not something introduced by
        // git-describe versioning — and it never reaches production
        // releases, since release CI sets `RECISDB_PROXY_VERSION` to the
        // clean tag name directly (see build.rs), bypassing `git describe`
        // entirely. Locked in here so a future change to this ranking is a
        // deliberate choice, not an accident.
        let releases = vec![release("v0.2.0", false, false)];
        let (stable, _) = select_updates("0.2.0-dirty", &releases);
        assert_eq!(stable.unwrap().tag, "v0.2.0");
    }

    #[test]
    fn select_updates_unparsable_current_treated_as_lowest_not_panicking() {
        // No tags reachable at all: `git describe --always` yields a bare
        // hash, which `parse_version` rejects. `select_updates` must not
        // panic, and falls back to "every valid release counts as an
        // update" (see its doc comment) rather than silently hiding them.
        let releases = vec![release("v0.1.0", false, false), release("v0.2.0-beta.1", true, false)];
        let (stable, prerelease) = select_updates("05a127c", &releases);
        assert_eq!(stable.unwrap().tag, "v0.1.0");
        assert_eq!(prerelease.unwrap().tag, "v0.2.0-beta.1");
    }

    // -- platform_supports_self_update / asset_filename -----------------------

    #[test]
    fn platform_support_matrix() {
        assert!(platform_supports_self_update("linux", "x86_64"));
        assert!(platform_supports_self_update("linux", "aarch64"));
        assert!(platform_supports_self_update("windows", "x86_64"));
        assert!(platform_supports_self_update("windows", "x86"));
        assert!(platform_supports_self_update("macos", "x86_64"));
        assert!(platform_supports_self_update("macos", "aarch64"));
        assert!(!platform_supports_self_update("linux", "arm")); // 32-bit ARM: no release asset
        assert!(!platform_supports_self_update("windows", "aarch64")); // no win-arm64 asset in CI
        assert!(!platform_supports_self_update("freebsd", "x86_64"));
    }

    #[test]
    fn asset_filename_matches_release_ci_naming() {
        assert_eq!(asset_filename("v1.2.3", "linux", "x86_64").as_deref(), Some("recisdb-proxy-v1.2.3-linux-amd64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "linux", "aarch64").as_deref(), Some("recisdb-proxy-v1.2.3-linux-arm64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "windows", "x86_64").as_deref(), Some("recisdb-v1.2.3-win-x64.zip"));
        assert_eq!(asset_filename("v1.2.3", "windows", "x86").as_deref(), Some("recisdb-v1.2.3-win-x86.zip"));
        assert_eq!(asset_filename("v1.2.3", "macos", "x86_64").as_deref(), Some("recisdb-proxy-v1.2.3-macos-amd64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "macos", "aarch64").as_deref(), Some("recisdb-proxy-v1.2.3-macos-arm64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "freebsd", "x86_64"), None);
    }

    #[test]
    fn is_target_binary_entry_matches_tail_component_only() {
        assert!(is_target_binary_entry("recisdb-proxy-v1.2.3-linux-amd64/recisdb-proxy", "linux"));
        assert!(!is_target_binary_entry("recisdb-proxy-v1.2.3-linux-amd64/recisdb-proxy.exe", "linux"));
        assert!(!is_target_binary_entry("recisdb-proxy-v1.2.3-linux-amd64/recisdb-proxy-setup", "linux"));
        assert!(is_target_binary_entry("recisdb-v1.2.3-win-x64\\recisdb-proxy.exe", "windows"));
        assert!(!is_target_binary_entry("recisdb-v1.2.3-win-x64\\recisdb.exe", "windows"));
        // macOS ships the same tarball layout as Linux.
        assert!(is_target_binary_entry("recisdb-proxy-v1.2.3-macos-arm64/recisdb-proxy", "macos"));
        assert!(!is_target_binary_entry("recisdb-proxy-v1.2.3-macos-arm64/recisdb-proxy-setup", "macos"));
    }

    #[test]
    fn magic_byte_validation() {
        assert!(has_valid_magic(b"\x7fELF", "linux"));
        assert!(!has_valid_magic(b"MZ\x00\x00", "linux"));
        assert!(has_valid_magic(b"MZ\x90\x00", "windows"));
        assert!(!has_valid_magic(b"\x7fELF", "windows"));
        assert!(!has_valid_magic(b"<htm", "linux"), "an HTML error page must never pass validation");

        // Mach-O 64-bit (both endiannesses) and the universal wrapper.
        assert!(has_valid_magic(&[0xcf, 0xfa, 0xed, 0xfe], "macos"));
        assert!(has_valid_magic(&[0xfe, 0xed, 0xfa, 0xcf], "macos"));
        assert!(has_valid_magic(&[0xca, 0xfe, 0xba, 0xbe], "macos"));
        // Cross-platform binaries must not pass as macOS ones.
        assert!(!has_valid_magic(b"\x7fELF", "macos"));
        assert!(!has_valid_magic(b"MZ\x90\x00", "macos"));
        assert!(!has_valid_magic(b"<htm", "macos"));
        // 32-bit Mach-O: no supported target produces it.
        assert!(!has_valid_magic(&[0xce, 0xfa, 0xed, 0xfe], "macos"));
    }

    // -- status_json -----------------------------------------------------

    #[test]
    fn status_json_shape() {
        assert_eq!(status_json(&UpdateStatus::Idle), json!({"state": "idle", "message": null}));
        assert_eq!(status_json(&UpdateStatus::Downloading), json!({"state": "downloading", "message": null}));
        assert_eq!(
            status_json(&UpdateStatus::Error("boom".to_string())),
            json!({"state": "error", "message": "boom"})
        );
    }
}
