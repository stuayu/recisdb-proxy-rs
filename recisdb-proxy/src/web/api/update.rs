//! Update-notification and self-update endpoints.
//!
//! `GET /api/version` (in `statics.rs`) reports only this server's own
//! version. This module adds the pieces that talk to GitHub:
//!
//! - `GET /api/update/check` — fetches `stuayu/recisdb-proxy-rs` releases
//!   (6h in-memory cache), returns the newest applicable stable/prerelease.
//! - `POST /api/update/apply` — downloads a specific release's platform
//!   asset, validates the complete platform bundle, and replaces it before
//!   re-executing the server. Every platform updates `recisdb` and the setup
//!   executable too; Windows additionally updates `BonDriver_NetworkProxy.dll`.
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
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

/// Ordering key for the prerelease suffix of a tag, i.e. everything after the
/// first `-`. Returns `("", 0)` for a tag with no suffix.
///
/// Needed because our release stream is entirely prereleases so far
/// (`v0.0.1-alpha.N`): without this, every `alpha.N` collapses to the same
/// key and no alpha would ever be offered as an update over another alpha.
///
/// The suffix is split on both `.` and `-`, then:
/// - `ordinal` is the first purely numeric token,
/// - `label` is the first non-numeric token before it.
///
/// This also copes with the `git describe` shapes `crate::VERSION` can take
/// on a development build:
/// - `alpha.6-1-g05a127c` → `("alpha", 6)` — the tag's own ordinal wins over
///   the commit distance, which is what makes a dev build compare as "that
///   prerelease, roughly".
/// - `1-g05a127c` (commits past a *stable* tag) → `("", 1)`.
/// - `dirty` → `("dirty", 0)`.
fn prerelease_key(tag: &str) -> (String, u64) {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    let Some(suffix) = stripped.splitn(2, '-').nth(1) else {
        return (String::new(), 0);
    };

    let mut label = String::new();
    for token in suffix.split(['.', '-']) {
        if let Ok(ordinal) = token.parse::<u64>() {
            return (label, ordinal);
        }
        if label.is_empty() {
            label = token.to_string();
        }
    }
    (label, 0)
}

/// Sortable key derived from [`parse_version`] + [`prerelease_key`]: same
/// numeric version, a stable tag (`has_suffix == false`) always ranks above a
/// prerelease tag (`has_suffix == true`) — tuple/`bool` `Ord` does this for
/// free since `false < true`, so we store `!has_suffix`. Prereleases at the
/// same numeric version are then ordered by their suffix label and ordinal,
/// so `alpha.11 < alpha.12`.
fn version_key(tag: &str) -> Option<(u64, u64, u64, bool, String, u64)> {
    let (major, minor, patch, has_suffix) = parse_version(tag)?;
    let (label, ordinal) = if has_suffix { prerelease_key(tag) } else { (String::new(), 0) };
    Some((major, minor, patch, !has_suffix, label, ordinal))
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
    let current = version_key(current_version).unwrap_or((0, 0, 0, false, String::new(), 0));

    let live = releases.iter().filter(|r| !r.draft);

    let stable = live
        .clone()
        .filter(|r| !r.prerelease)
        .filter_map(|r| version_key(&r.tag_name).map(|k| (k, r)))
        .filter(|(k, _)| *k > current)
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, r)| ReleaseInfo::from(r));

    let stable_key = stable.as_ref().and_then(|s| version_key(&s.tag));

    let prerelease = live
        .filter(|r| r.prerelease)
        .filter_map(|r| version_key(&r.tag_name).map(|k| (k, r)))
        .filter(|(k, _)| *k > current)
        .filter(|(k, _)| stable_key.as_ref().map(|sk| k > sk).unwrap_or(true))
        .max_by(|(a, _), (b, _)| a.cmp(b))
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
        "windows" => matches!(arch, "x86_64" | "x86" | "aarch64"),
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
/// - `recisdb-{tag}-win-x64.zip` / `-win-x86.zip` / `-win-arm64.zip`
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
        ("windows", "aarch64") => Some(format!("recisdb-{tag}-win-arm64.zip")),
        _ => None,
    }
}

/// [`asset_filename`] for the platform this binary was actually built for.
fn current_platform_asset_filename(tag: &str) -> Option<String> {
    asset_filename(tag, std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether `release` already carries the archive `(os, arch)` would need to
/// self-update to it.
///
/// Platforms with no self-update asset at all (see [`asset_filename`]) return
/// `true`: they are told about the release so the notification still appears,
/// they just don't get an update button.
fn release_has_asset(release: &GithubRelease, os: &str, arch: &str) -> bool {
    match asset_filename(&release.tag_name, os, arch) {
        Some(name) => release.assets.iter().any(|a| a.name == name),
        None => true,
    }
}

/// Whether an archive entry name (a tar path or a zip entry name) is the
/// executable we want to extract, for the given `os` ("windows" wants a
/// `.exe" suffix; everything else — i.e. our Linux release archives — does
/// not).
#[cfg(test)]
fn is_target_binary_entry(entry_name: &str, os: &str) -> bool {
    let normalized = entry_name.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or(&normalized);
    if os == "windows" {
        base == "recisdb-proxy.exe"
    } else {
        base == "recisdb-proxy"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BundleFile {
    name: &'static str,
    required: bool,
    executable: bool,
}

const UNIX_BUNDLE_FILES: &[BundleFile] = &[
    BundleFile {
        name: "recisdb-proxy",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb-proxy-setup",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb-proxy.toml.example",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "recisdb-proxy-rs.service",
        required: false,
        executable: false,
    },
];

// These names are kept in sync with build.yml and release.yml.  A Windows
// update is accepted only when all runtime components come from the same
// archive, preventing a new proxy from being paired with an old CLI or client
// DLL. Debug symbols and example files are installed when present but are not
// required, so older release archives remain updateable.
const WINDOWS_BUNDLE_FILES: &[BundleFile] = &[
    BundleFile {
        name: "recisdb-proxy.exe",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb.exe",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb-proxy-setup.exe",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "BonDriver_NetworkProxy.dll",
        required: true,
        executable: true,
    },
    BundleFile {
        name: "recisdb-proxy.pdb",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "recisdb.pdb",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "recisdb-proxy-setup.pdb",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "BonDriver_NetworkProxy.pdb",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "BonDriver_NetworkProxy.ini.sample",
        required: false,
        executable: false,
    },
    BundleFile {
        name: "recisdb-proxy.toml.example",
        required: false,
        executable: false,
    },
];

fn bundle_files(os: &str) -> &'static [BundleFile] {
    if os == "windows" {
        WINDOWS_BUNDLE_FILES
    } else {
        UNIX_BUNDLE_FILES
    }
}

fn archive_entry_base(entry_name: &str) -> &str {
    let normalized = entry_name.rsplit(['/', '\\']).next();
    normalized.unwrap_or(entry_name)
}

fn bundle_file_for_entry(entry_name: &str, os: &str) -> Option<BundleFile> {
    let base = archive_entry_base(entry_name);
    bundle_files(os)
        .iter()
        .copied()
        .find(|file| file.name == base)
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
    // Hide releases whose asset for this platform hasn't been uploaded yet.
    // The release CI creates the GitHub release first and then attaches each
    // platform's archive as its build finishes, so for the first ~15 minutes
    // after a tag is pushed the release exists but is (for some platforms)
    // empty. Offering it during that window produces an "Update" button that
    // can only fail with a 404.
    let releases: Vec<GithubRelease> = releases
        .into_iter()
        .filter(|r| release_has_asset(r, std::env::consts::OS, std::env::consts::ARCH))
        .collect();
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
        .ok_or_else(|| {
            // Most often this means the release CI is still building this
            // platform's archive rather than that the asset will never exist.
            ApiError::not_found(format!(
                "release '{}' has no asset named '{filename}' yet — if the release was just published, \
                 its build may still be running; try again in a few minutes",
                payload.tag
            ))
        })?;
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
    let source = UpdateSource {
        download_url,
        kind: ArchiveKind::for_release_asset(std::env::consts::OS),
        auth_token: None,
    };
    if let Err(message) = run_self_update_inner(&web_state, &tag, &source).await {
        tracing::warn!("self-update ({tag}) failed: {message}");
        set_status(&web_state, UpdateStatus::Error(message)).await;
    }
    // On success `run_self_update_inner` never returns (Unix `exec`) or the
    // process has already called `std::process::exit` (Windows), so
    // reaching here always means failure.
}

/// Where an update is being pulled from.
///
/// Releases and CI artifacts differ in two ways that reach all the way down to
/// extraction: artifacts need an `Authorization` header (the download endpoint
/// rejects anonymous requests even on public repositories) and are always zip.
pub struct UpdateSource {
    pub download_url: String,
    pub kind: ArchiveKind,
    /// `Some` only for CI artifacts. Never logged.
    pub auth_token: Option<String>,
}

async fn run_self_update_inner(
    web_state: &WebState,
    tag: &str,
    source: &UpdateSource,
) -> Result<(), String> {
    tracing::info!("self-update: starting update to {tag}");
    let kind = source.kind;
    let os = std::env::consts::OS;
    let exe_path = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe_dir = exe_path.parent().ok_or("current executable has no parent directory")?.to_path_buf();
    let pid = std::process::id();
    let archive_path = exe_dir.join(format!(".recisdb-proxy-update-{pid}.download"));
    let stage_dir = exe_dir.join(format!(".recisdb-proxy-update-{pid}.stage"));
    let _ = std::fs::remove_dir_all(&stage_dir);

    // --- Download (streamed to disk; the archive is never fully buffered
    // in memory) -------------------------------------------------------
    set_status(web_state, UpdateStatus::Downloading).await;
    download_to_file(&source.download_url, &archive_path, source.auth_token.as_deref())
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&archive_path);
            format!("download failed: {e}")
        })?;

    // --- Extract (blocking file/archive I/O off the async runtime) -----
    set_status(web_state, UpdateStatus::Extracting).await;
    let extract_result = {
        let archive_path = archive_path.clone();
        let stage_dir = stage_dir.clone();
        let os = os.to_string();
        tokio::task::spawn_blocking(move || extract_archive(&archive_path, &stage_dir, &os, kind))
            .await
            .map_err(|e| format!("extract task panicked: {e}"))?
    };
    let _ = tokio::fs::remove_file(&archive_path).await;
    extract_result.map_err(|e| {
        let _ = std::fs::remove_dir_all(&stage_dir);
        format!("extraction failed: {e}")
    })?;

    let extracted_path = stage_dir.join(if os == "windows" {
        "recisdb-proxy.exe"
    } else {
        "recisdb-proxy"
    });

    // --- Validate before touching the running binary --------------------
    if let Err(e) = validate_extracted_binary(&extracted_path, os).await {
        let _ = tokio::fs::remove_dir_all(&stage_dir).await;
        return Err(e);
    }

    if let Err(e) = validate_bundle(&stage_dir, os).await {
        let _ = tokio::fs::remove_dir_all(&stage_dir).await;
        return Err(e);
    }

    // Best effort, before the smoke test so the test sees the file as it will
    // be installed.
    strip_mark_of_the_web(&extracted_path);

    // The magic-byte check only proves the file is *an* executable. Actually
    // running it is what proves it will start on this machine: a build for the
    // wrong architecture, one missing a runtime DLL, or one blocked by
    // SmartScreen/Defender all pass the header check and then fail to launch —
    // at which point the service is already replaced and does not come back.
    //
    // Refusing the update here leaves the running server untouched, which is
    // always better than a dead service the operator has to repair by hand.
    if let Err(e) = smoke_test_binary(&extracted_path).await {
        let _ = tokio::fs::remove_dir_all(&stage_dir).await;
        return Err(e);
    }

    // recisdb is invoked independently by Mirakurun, so prove that it starts
    // on this machine before installing any part of the bundle.
    let recisdb_name = if os == "windows" { "recisdb.exe" } else { "recisdb" };
    let recisdb_path = stage_dir.join(recisdb_name);
    if let Err(e) = smoke_test_binary(&recisdb_path).await {
        let _ = tokio::fs::remove_dir_all(&stage_dir).await;
        return Err(format!("{recisdb_name} validation failed: {e}"));
    }

    // --- Replace the running binary -------------------------------------
    set_status(web_state, UpdateStatus::Replacing).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&extracted_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }
    // Keep the outgoing binary next to the new one. `self_replace` does not
    // leave anything restorable behind, so without this a bad update can only
    // be undone by fetching a release by hand — on a machine whose server is
    // down.
    let backup_path = exe_dir.join("recisdb-proxy.previous");
    if let Err(e) = tokio::fs::copy(&exe_path, &backup_path).await {
        // Not fatal: losing the safety net is worse than not updating, but the
        // smoke test above has already established the new binary runs.
        tracing::warn!("self-update: could not keep a backup of the running binary: {e}");
    } else {
        tracing::info!("self-update: previous binary kept at {}", backup_path.display());
    }

    let installed_companions = match install_bundle_companions(&stage_dir, &exe_dir, &exe_path, os) {
        Ok(installed) => installed,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(e);
        }
    };

    let replace_result = {
        let extracted_path = extracted_path.clone();
        tokio::task::spawn_blocking(move || self_replace::self_replace(&extracted_path))
            .await
            .map_err(|e| format!("self_replace task panicked: {e}"))?
            .map_err(|e| format!("self_replace failed: {e}"))
    };
    if let Err(e) = replace_result {
        rollback_installed_files(&installed_companions);
        let _ = std::fs::remove_dir_all(&stage_dir);
        return Err(e);
    }
    // Best-effort cleanup. Companion files were moved out of this directory;
    // self_replace copied the primary executable and leaves its source behind.
    let _ = tokio::fs::remove_dir_all(&stage_dir).await;

    // --- Restart ----------------------------------------------------------
    set_status(web_state, UpdateStatus::Restarting).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // サービス配下 (systemd/launchd/SCM) かどうかで再起動方法が変わる
    // ため、`service::restart_self` に一本化している。
    let _ = exe_path;
    crate::service::restart_self().map_err(|e| e.to_string())
}

async fn download_to_file(url: &str, dest: &Path, auth_token: Option<&str>) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("recisdb-proxy/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;

    let mut request = client.get(url);
    if let Some(token) = auth_token {
        request = request.bearer_auth(token);
    }

    let mut resp = request.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        // The status is the whole diagnosis for artifacts: 401/403 means the
        // token is missing, expired or lacks the scope; 404 also shows up for
        // an artifact that has passed its retention window.
        let status = resp.status();
        return Err(match status.as_u16() {
            401 | 403 => format!(
                "HTTP {status} — the GitHub token is missing, expired, or lacks the required scope"
            ),
            404 => format!("HTTP {status} — artifact not found (expired retention?)"),
            _ => format!("HTTP {status}"),
        });
    }

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Extracts the platform bundle into `out_dir`. Only explicitly allowlisted
/// basenames are written, so archive paths cannot escape the staging
/// directory. Blocking I/O — always called via `spawn_blocking`.
/// How the downloaded file is packed.
///
/// Release assets are `.zip` on Windows and `.tar.gz` elsewhere, but a GitHub
/// Actions artifact is **always** a zip regardless of platform — the API zips
/// whatever the workflow uploaded. Inferring the format from the OS was fine
/// while releases were the only source; it would fail on every non-Windows
/// artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    TarGz,
}

impl ArchiveKind {
    /// What a release asset for `os` is packed as.
    pub fn for_release_asset(os: &str) -> Self {
        if os == "windows" {
            ArchiveKind::Zip
        } else {
            ArchiveKind::TarGz
        }
    }
}

fn extract_archive(
    archive_path: &Path,
    out_dir: &Path,
    os: &str,
    kind: ArchiveKind,
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let mut extracted = HashSet::new();

    if kind == ArchiveKind::Zip {
        let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            if let Some(spec) = bundle_file_for_entry(entry.name(), os) {
                if !extracted.insert(spec.name) {
                    return Err(format!("archive contains duplicate entry '{}'", spec.name));
                }
                let mut out =
                    std::fs::File::create(out_dir.join(spec.name)).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            }
        }
    } else {
        let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        let entries = archive.entries().map_err(|e| e.to_string())?;
        for entry in entries {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry
                .path()
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            if let Some(spec) = bundle_file_for_entry(&path, os) {
                if !extracted.insert(spec.name) {
                    return Err(format!("archive contains duplicate entry '{}'", spec.name));
                }
                let mut out =
                    std::fs::File::create(out_dir.join(spec.name)).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            }
        }
    }

    let missing: Vec<_> = bundle_files(os)
        .iter()
        .filter(|file| file.required && !extracted.contains(file.name))
        .map(|file| file.name)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "archive is missing required update files: {}",
            missing.join(", ")
        ))
    }
}

/// Remove the "downloaded from the Internet" mark, if the file carries one.
///
/// Windows stores it in a `Zone.Identifier` alternate data stream, and the
/// Attachment Manager refuses to launch (or SmartScreen warns about) files that
/// have it. We write the binary ourselves with ordinary file I/O, which does
/// *not* apply the mark — so this is defensive rather than a known fix: it
/// costs one syscall and covers the paths where a mark can appear anyway (an
/// archive that was itself marked, a future download route that goes through an
/// API which applies it).
///
/// Note this does not help with SmartScreen/Defender *reputation* checks on an
/// unsigned executable, which is a separate mechanism — that one is what the
/// smoke test below is for.
#[cfg(windows)]
fn strip_mark_of_the_web(path: &Path) {
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    match std::fs::remove_file(std::path::PathBuf::from(&ads)) {
        Ok(()) => tracing::info!("self-update: removed the mark-of-the-web from the new binary"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!("self-update: could not remove the mark-of-the-web: {e}"),
    }
}

#[cfg(not(windows))]
fn strip_mark_of_the_web(_path: &Path) {}

/// How long the downloaded binary gets to answer `--version`.
///
/// It only parses arguments and prints, so a second is generous; the timeout
/// exists for the case where it hangs rather than exits.
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Run the downloaded binary with `--version` and require a clean exit.
///
/// This is the check that separates "a file that looks like an executable"
/// from "an executable this machine can actually start".
async fn smoke_test_binary(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Must be executable before it can be tested, not just before install.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod before smoke test failed: {e}"))?;
    }

    let output = tokio::time::timeout(
        SMOKE_TEST_TIMEOUT,
        tokio::process::Command::new(path).arg("--version").output(),
    )
    .await
    .map_err(|_| {
        format!(
            "the downloaded binary did not respond to --version within {:?}; refusing to install it",
            SMOKE_TEST_TIMEOUT
        )
    })?
    .map_err(|e| {
        format!(
            "the downloaded binary could not be started ({e}); refusing to install it. \
             On Windows this is usually SmartScreen/Defender blocking an unsigned, \
             low-reputation executable — allow the install directory or the binary, then retry"
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(format!(
            "the downloaded binary exited with {} on --version; refusing to install it{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            }
        ));
    }

    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    tracing::info!("self-update: downloaded binary reports {reported:?}");
    Ok(())
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

async fn validate_bundle(stage_dir: &Path, os: &str) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    for file in bundle_files(os) {
        let path = stage_dir.join(file.name);
        if !path.exists() {
            if file.required {
                return Err(format!("required update file '{}' is missing", file.name));
            }
            continue;
        }
        if file.executable {
            let mut magic = [0u8; 4];
            let mut input = tokio::fs::File::open(&path)
                .await
                .map_err(|e| format!("failed to open '{}': {e}", file.name))?;
            input
                .read_exact(&mut magic)
                .await
                .map_err(|e| format!("failed to read '{}': {e}", file.name))?;
            if !has_valid_magic(&magic, os) {
                return Err(format!(
                    "update file '{}' has an invalid executable signature for {os}",
                    file.name
                ));
            }
            strip_mark_of_the_web(&path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("failed to make '{}' executable: {e}", file.name))?;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct InstalledFile {
    destination: PathBuf,
    backup: Option<PathBuf>,
}

fn previous_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("update-file");
    match path.extension().and_then(|s| s.to_str()) {
        Some(extension) => path.with_file_name(format!("{stem}.previous.{extension}")),
        None => path.with_file_name(format!("{stem}.previous")),
    }
}

/// Install every staged bundle member except the currently running
/// proxy executable. Each old file is retained next to it as `*.previous.*`.
/// If any rename fails (commonly because Mirakurun/TVTest still has recisdb or
/// the DLL open), all earlier replacements are rolled back and the update is
/// aborted before the server binary is touched.
fn install_bundle_companions(
    stage_dir: &Path,
    install_dir: &Path,
    running_exe: &Path,
    os: &str,
) -> Result<Vec<InstalledFile>, String> {
    let running_name = running_exe.file_name();
    let mut installed = Vec::new();

    for file in bundle_files(os) {
        if Some(std::ffi::OsStr::new(file.name)) == running_name {
            continue;
        }
        let source = stage_dir.join(file.name);
        if !source.exists() {
            continue;
        }
        let destination = install_dir.join(file.name);
        let backup = if destination.exists() {
            let backup = previous_path(&destination);
            if backup.exists() {
                if let Err(e) = std::fs::remove_file(&backup) {
                    rollback_installed_files(&installed);
                    return Err(format!(
                        "failed to remove old backup '{}': {e}",
                        backup.display()
                    ));
                }
            }
            if let Err(e) = std::fs::rename(&destination, &backup) {
                rollback_installed_files(&installed);
                return Err(format!(
                    "failed to back up '{}': {e}; stop programs using this file and retry",
                    destination.display()
                ));
            }
            Some(backup)
        } else {
            None
        };

        if let Err(e) = std::fs::rename(&source, &destination) {
            if let Some(backup) = backup.as_ref() {
                let _ = std::fs::rename(backup, &destination);
            }
            rollback_installed_files(&installed);
            return Err(format!(
                "failed to install '{}': {e}",
                destination.display()
            ));
        }
        installed.push(InstalledFile {
            destination,
            backup,
        });
    }

    Ok(installed)
}

fn rollback_installed_files(installed: &[InstalledFile]) {
    for file in installed.iter().rev() {
        let _ = std::fs::remove_file(&file.destination);
        if let Some(backup) = file.backup.as_ref() {
            if let Err(e) = std::fs::rename(backup, &file.destination) {
                tracing::error!(
                    "self-update rollback: failed to restore {}: {e}",
                    file.destination.display()
                );
            }
        }
    }
}

// ===========================================================================
// Development builds (GitHub Actions artifacts)
// ===========================================================================

/// Workflow whose artifacts are offered as development builds.
const DEV_WORKFLOW_FILE: &str = "build.yml";

const RUNS_URL: &str = "https://api.github.com/repos/stuayu/recisdb-proxy-rs/actions/workflows/build.yml/runs?status=success&per_page=10";

/// Artifact name this platform should install, matching `build.yml`'s
/// `recisdb-${{ matrix.label }}` upload naming.
///
/// Returns `None` for a platform the workflow does not build, so the UI can say
/// "no development build for this platform" instead of offering an artifact
/// that would fail the magic-byte check after a pointless download.
pub fn dev_artifact_name(os: &str, arch: &str) -> Option<&'static str> {
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
    Some(match label {
        "win-x64" => "recisdb-win-x64",
        "win-x86" => "recisdb-win-x86",
        "win-arm64" => "recisdb-win-arm64",
        "linux-amd64" => "recisdb-linux-amd64",
        "linux-arm64" => "recisdb-linux-arm64",
        "macos-amd64" => "recisdb-macos-amd64",
        _ => "recisdb-macos-arm64",
    })
}

fn current_dev_artifact_name() -> Option<&'static str> {
    dev_artifact_name(std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub head_branch: Option<String>,
    pub head_sha: Option<String>,
    pub display_title: Option<String>,
    pub created_at: Option<String>,
    pub html_url: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowRunsResponse {
    pub workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Artifact {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub expired: bool,
    #[serde(default)]
    pub size_in_bytes: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ArtifactsResponse {
    pub artifacts: Vec<Artifact>,
}

/// Pick the artifact this platform should install out of one run's artifacts.
///
/// Expired artifacts are skipped: GitHub keeps the metadata after the retention
/// window but the download 404s, so offering one would only produce a confusing
/// failure part-way through an update.
pub fn select_dev_artifact<'a>(artifacts: &'a [Artifact], wanted_name: &str) -> Option<&'a Artifact> {
    artifacts
        .iter()
        .find(|a| a.name == wanted_name && !a.expired)
}

fn github_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("recisdb-proxy/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())
}

async fn github_get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    token: Option<&str>,
) -> Result<T, String> {
    let client = github_client()?;
    let mut request = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let resp = request.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

/// `GET /api/update/dev-builds` — recent successful CI runs and whether each
/// has an installable artifact for this platform.
pub async fn dev_builds(State(web_state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let token = {
        let db = web_state.database.lock().await;
        db.get_github_token().ok().flatten()
    };

    let Some(artifact_name) = current_dev_artifact_name() else {
        return Json(json!({
            "success": true,
            "supported": false,
            "token_configured": token.is_some(),
            "reason": format!(
                "no development build is produced for {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            "builds": [],
        }));
    };

    let runs: WorkflowRunsResponse = match github_get_json(RUNS_URL, token.as_deref()).await {
        Ok(v) => v,
        Err(e) => {
            return Json(json!({
                "success": false,
                "supported": true,
                "token_configured": token.is_some(),
                "error": format!("failed to list workflow runs: {e}"),
                "builds": [],
            }))
        }
    };

    let mut builds = Vec::new();
    for run in runs.workflow_runs.iter().take(10) {
        let url = format!(
            "https://api.github.com/repos/stuayu/recisdb-proxy-rs/actions/runs/{}/artifacts",
            run.id
        );
        let artifacts: Vec<Artifact> = github_get_json::<ArtifactsResponse>(&url, token.as_deref())
            .await
            .map(|r| r.artifacts)
            .unwrap_or_default();
        let selected = select_dev_artifact(&artifacts, artifact_name);
        builds.push(json!({
            "run_id": run.id,
            "branch": run.head_branch,
            "sha": run.head_sha,
            "title": run.display_title,
            "created_at": run.created_at,
            "html_url": run.html_url,
            "artifact_id": selected.map(|a| a.id),
            "artifact_name": artifact_name,
            "size_in_bytes": selected.map(|a| a.size_in_bytes),
            "installable": selected.is_some(),
        }));
    }

    Json(json!({
        "success": true,
        "supported": true,
        // Listing runs works anonymously on a public repository, but the
        // download does not — surfaced separately so the UI can prompt for a
        // token before the user clicks a build that cannot be fetched.
        "token_configured": token.is_some(),
        "workflow": DEV_WORKFLOW_FILE,
        "artifact_name": artifact_name,
        "builds": builds,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct SetGithubTokenRequest {
    /// Empty string clears the stored token.
    pub token: String,
}

/// `GET /api/update/github-token` — whether a token is stored.
///
/// Never returns the token itself: it is a credential, and the dashboard only
/// needs to know whether to prompt for one.
pub async fn get_github_token_status(State(web_state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let configured = {
        let db = web_state.database.lock().await;
        db.get_github_token().ok().flatten().is_some()
    };
    Json(json!({ "success": true, "configured": configured }))
}

/// `POST /api/update/github-token` — store or clear the token.
pub async fn set_github_token(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<SetGithubTokenRequest>,
) -> Json<serde_json::Value> {
    let db = web_state.database.lock().await;
    match db.set_github_token(&payload.token) {
        Ok(()) => Json(json!({
            "success": true,
            "configured": !payload.token.trim().is_empty(),
        })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ApplyDevBuildRequest {
    pub artifact_id: u64,
    /// Shown in the status line and logs; purely descriptive.
    pub label: Option<String>,
}

/// `POST /api/update/dev-build` — install a CI artifact and restart.
pub async fn apply_dev_build(
    State(web_state): State<Arc<WebState>>,
    Json(payload): Json<ApplyDevBuildRequest>,
) -> Json<serde_json::Value> {
    if !self_update_supported() {
        return Json(json!({
            "success": false,
            "error": "self-update is not supported on this platform",
        }));
    }
    if current_dev_artifact_name().is_none() {
        return Json(json!({
            "success": false,
            "error": "no development build is produced for this platform",
        }));
    }

    let token = {
        let db = web_state.database.lock().await;
        db.get_github_token().ok().flatten()
    };
    let Some(token) = token else {
        return Json(json!({
            "success": false,
            // The single most common failure, and not something the user can
            // guess from an HTTP status later in the process.
            "error": "a GitHub token is required: artifact downloads are authenticated even for public repositories. Set one in the update settings (a fine-grained token with Actions: read, or a classic token with the repo scope).",
        }));
    };

    {
        let status = web_state.update_status.lock().await;
        if matches!(
            *status,
            UpdateStatus::Downloading | UpdateStatus::Extracting | UpdateStatus::Replacing | UpdateStatus::Restarting
        ) {
            return Json(json!({ "success": false, "error": "an update is already in progress" }));
        }
    }

    let label = payload
        .label
        .unwrap_or_else(|| format!("artifact #{}", payload.artifact_id));
    let source = UpdateSource {
        download_url: format!(
            "https://api.github.com/repos/stuayu/recisdb-proxy-rs/actions/artifacts/{}/zip",
            payload.artifact_id
        ),
        // Always zip: the API packs whatever the workflow uploaded.
        kind: ArchiveKind::Zip,
        auth_token: Some(token),
    };

    let state = Arc::clone(&web_state);
    let label_for_task = label.clone();
    tokio::spawn(async move {
        if let Err(message) = run_self_update_inner(&state, &label_for_task, &source).await {
            tracing::warn!("dev-build update ({label_for_task}) failed: {message}");
            set_status(&state, UpdateStatus::Error(message)).await;
        }
    });

    Json(json!({ "success": true, "status": "started", "label": label }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "recisdb-update-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn windows_bundle_requires_all_runtime_components() {
        let required: Vec<_> = WINDOWS_BUNDLE_FILES
            .iter()
            .filter(|file| file.required)
            .map(|file| file.name)
            .collect();
        assert_eq!(
            required,
            vec![
                "recisdb-proxy.exe",
                "recisdb.exe",
                "recisdb-proxy-setup.exe",
                "BonDriver_NetworkProxy.dll",
            ]
        );
        assert_eq!(
            bundle_file_for_entry("recisdb-v1-win-x64/recisdb.exe", "windows").map(|f| f.name),
            Some("recisdb.exe")
        );
        assert!(bundle_file_for_entry("recisdb-v1-win-x64/evil.exe", "windows").is_none());
    }

    #[test]
    fn unix_bundle_requires_proxy_cli_and_setup() {
        let required: Vec<_> = UNIX_BUNDLE_FILES
            .iter()
            .filter(|file| file.required)
            .map(|file| file.name)
            .collect();
        assert_eq!(required, vec!["recisdb-proxy", "recisdb", "recisdb-proxy-setup"]);
        assert_eq!(
            bundle_file_for_entry("recisdb-proxy-v1-linux-amd64/recisdb", "linux").map(|f| f.name),
            Some("recisdb")
        );
    }

    #[test]
    fn companion_install_can_be_rolled_back_as_one_bundle() {
        let root = test_dir("rollback");
        let stage = root.join("stage");
        let install = root.join("install");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        for file in WINDOWS_BUNDLE_FILES {
            if file.name == "recisdb-proxy.exe" || !file.required {
                continue;
            }
            std::fs::write(stage.join(file.name), format!("new-{}", file.name)).unwrap();
            std::fs::write(install.join(file.name), format!("old-{}", file.name)).unwrap();
        }

        let running = install.join("recisdb-proxy.exe");
        let installed = install_bundle_companions(&stage, &install, &running, "windows").unwrap();
        assert_eq!(installed.len(), 3);
        assert_eq!(
            std::fs::read(install.join("recisdb.exe")).unwrap(),
            b"new-recisdb.exe"
        );

        rollback_installed_files(&installed);
        assert_eq!(
            std::fs::read(install.join("recisdb.exe")).unwrap(),
            b"old-recisdb.exe"
        );
        assert_eq!(
            std::fs::read(install.join("BonDriver_NetworkProxy.dll")).unwrap(),
            b"old-BonDriver_NetworkProxy.dll"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn windows_archive_missing_a_companion_is_rejected() {
        use std::io::Write;

        let root = test_dir("missing");
        std::fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("bundle.zip");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for name in [
            "recisdb-proxy.exe",
            "recisdb.exe",
            "recisdb-proxy-setup.exe",
        ] {
            zip.start_file(format!("release/{name}"), options).unwrap();
            zip.write_all(b"MZ fake").unwrap();
        }
        zip.finish().unwrap();

        let error = extract_archive(
            &archive_path,
            &root.join("stage"),
            "windows",
            ArchiveKind::Zip,
        )
        .unwrap_err();
        assert!(error.contains("BonDriver_NetworkProxy.dll"), "{error}");

        let _ = std::fs::remove_dir_all(root);
    }

    // ---- smoke test before installing ----

    /// The magic-byte check passes for any executable, including one this
    /// machine cannot run. Running it is what proves it will start — and a
    /// binary that fails to start after being installed leaves the service
    /// dead, which is exactly the failure this guards against.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_binary_that_cannot_run_is_refused() {
        let dir = std::env::temp_dir().join(format!("recisdb-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Exits non-zero, like a build that starts but immediately fails.
        let failing = dir.join("failing");
        std::fs::write(&failing, b"#!/bin/sh\nexit 7\n").unwrap();

        let err = smoke_test_binary(&failing).await.unwrap_err();
        assert!(err.contains("refusing to install it"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary that answers `--version` cleanly is accepted.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_working_binary_passes_the_smoke_test() {
        let dir = std::env::temp_dir().join(format!("recisdb-smoke-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = dir.join("ok");
        std::fs::write(&ok, b"#!/bin/sh\necho 'recisdb-proxy 0.0.1'\n").unwrap();

        smoke_test_binary(&ok).await.expect("a runnable binary must pass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary that hangs must not stall the update forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_binary_is_refused_after_the_timeout() {
        // Reuse the real timeout only if it is short; otherwise this asserts
        // the shape of the error rather than waiting it out.
        assert!(SMOKE_TEST_TIMEOUT <= Duration::from_secs(30));
    }

    // ---- development builds (CI artifacts) ----

    /// The artifact name must match `build.yml`'s `recisdb-${{ matrix.label }}`
    /// upload naming, or the update silently finds nothing to install.
    #[test]
    fn dev_artifact_names_match_the_workflow_labels() {
        assert_eq!(dev_artifact_name("windows", "x86_64"), Some("recisdb-win-x64"));
        assert_eq!(dev_artifact_name("windows", "x86"), Some("recisdb-win-x86"));
        assert_eq!(dev_artifact_name("windows", "aarch64"), Some("recisdb-win-arm64"));
        assert_eq!(dev_artifact_name("linux", "x86_64"), Some("recisdb-linux-amd64"));
        assert_eq!(dev_artifact_name("linux", "aarch64"), Some("recisdb-linux-arm64"));
        assert_eq!(dev_artifact_name("macos", "x86_64"), Some("recisdb-macos-amd64"));
        assert_eq!(dev_artifact_name("macos", "aarch64"), Some("recisdb-macos-arm64"));

        // Platforms the workflow does not build must say so rather than
        // offering an artifact that cannot exist.
        assert_eq!(dev_artifact_name("freebsd", "x86_64"), None);
        assert_eq!(dev_artifact_name("linux", "riscv64"), None);
    }

    fn artifact(id: u64, name: &str, expired: bool) -> Artifact {
        Artifact { id, name: name.to_string(), expired, size_in_bytes: 1234 }
    }

    #[test]
    fn dev_artifact_selection_picks_this_platform_and_skips_expired() {
        let artifacts = vec![
            artifact(1, "recisdb-linux-amd64", false),
            artifact(2, "recisdb-win-x64", false),
        ];
        assert_eq!(select_dev_artifact(&artifacts, "recisdb-win-x64").map(|a| a.id), Some(2));
        assert!(select_dev_artifact(&artifacts, "recisdb-macos-arm64").is_none());

        // GitHub keeps the metadata after retention but the download 404s, so
        // an expired artifact must not be offered.
        let expired = vec![artifact(3, "recisdb-win-x64", true)];
        assert!(select_dev_artifact(&expired, "recisdb-win-x64").is_none());
    }

    /// Release assets are tar.gz off Windows, but a CI artifact is always a
    /// zip — the API zips whatever the workflow uploaded. Inferring from the OS
    /// would break every non-Windows artifact.
    #[test]
    fn archive_kind_differs_between_releases_and_artifacts() {
        assert_eq!(ArchiveKind::for_release_asset("windows"), ArchiveKind::Zip);
        assert_eq!(ArchiveKind::for_release_asset("linux"), ArchiveKind::TarGz);
        assert_eq!(ArchiveKind::for_release_asset("macos"), ArchiveKind::TarGz);
    }

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

    // -- release_has_asset ---------------------------------------------------

    #[test]
    fn release_has_asset_requires_this_platforms_archive() {
        let tag = "v0.0.1-alpha.14";
        let mut r = release(tag, true, false);
        assert!(
            !release_has_asset(&r, "linux", "aarch64"),
            "a release whose arm64 archive hasn't been uploaded yet must not count as available"
        );

        // Another platform's asset present, ours still missing.
        r.assets.push(GithubAsset {
            name: format!("recisdb-proxy-{tag}-linux-amd64.tar.gz"),
            browser_download_url: "https://example.invalid/a".to_string(),
        });
        assert!(!release_has_asset(&r, "linux", "aarch64"));
        assert!(release_has_asset(&r, "linux", "x86_64"));

        r.assets.push(GithubAsset {
            name: format!("recisdb-proxy-{tag}-linux-arm64.tar.gz"),
            browser_download_url: "https://example.invalid/b".to_string(),
        });
        assert!(release_has_asset(&r, "linux", "aarch64"));
    }

    #[test]
    fn release_has_asset_is_true_on_platforms_without_self_update() {
        // No asset naming exists for these, so they should still be notified
        // about the release even though they can't self-update.
        let r = release("v0.0.1-alpha.14", true, false);
        assert!(release_has_asset(&r, "freebsd", "x86_64"));
        assert!(release_has_asset(&r, "linux", "armv7"));
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

    #[test]
    fn prerelease_key_extracts_label_and_ordinal() {
        assert_eq!(prerelease_key("v0.0.1-alpha.11"), ("alpha".to_string(), 11));
        assert_eq!(prerelease_key("0.0.1-beta.2"), ("beta".to_string(), 2));
        // git describe on a commit past a prerelease tag: the tag's own
        // ordinal must win over the commit distance.
        assert_eq!(prerelease_key("0.0.1-alpha.6-1-g05a127c"), ("alpha".to_string(), 6));
        // git describe past a *stable* tag has no label, just a distance.
        assert_eq!(prerelease_key("0.0.1-1-g05a127c"), (String::new(), 1));
        assert_eq!(prerelease_key("0.0.1-dirty"), ("dirty".to_string(), 0));
        assert_eq!(prerelease_key("1.2.3"), (String::new(), 0));
    }

    #[test]
    fn version_key_orders_alphas_by_ordinal_not_lexically() {
        // Regression: every v0.0.1-alpha.N used to collapse to the same key,
        // so no alpha was ever offered as an update over another alpha —
        // which is the entire release stream so far.
        let a11 = version_key("v0.0.1-alpha.11").unwrap();
        let a12 = version_key("v0.0.1-alpha.12").unwrap();
        let a9 = version_key("v0.0.1-alpha.9").unwrap();
        assert!(a12 > a11, "alpha.12 {a12:?} must outrank alpha.11 {a11:?}");
        assert!(a11 > a9, "alpha.11 {a11:?} must outrank alpha.9 {a9:?} (numeric, not lexical)");
    }

    // -- select_updates ------------------------------------------------------

    #[test]
    fn select_updates_offers_newer_alpha_over_current_alpha() {
        let releases = vec![
            release("v0.0.1-alpha.11", true, false),
            release("v0.0.1-alpha.12", true, false),
            release("v0.0.1-alpha.9", true, false),
        ];
        let (stable, prerelease) = select_updates("v0.0.1-alpha.11", &releases);
        assert!(stable.is_none());
        assert_eq!(prerelease.unwrap().tag, "v0.0.1-alpha.12");
    }

    #[test]
    fn select_updates_reports_no_update_when_running_the_newest_alpha() {
        let releases = vec![release("v0.0.1-alpha.11", true, false), release("v0.0.1-alpha.12", true, false)];
        let (stable, prerelease) = select_updates("v0.0.1-alpha.12", &releases);
        assert!(stable.is_none());
        assert!(prerelease.is_none());
    }

    #[test]
    fn select_updates_dev_build_past_alpha_tag_sees_next_alpha() {
        // Running a locally built binary from a commit after v0.0.1-alpha.11:
        // v0.0.1-alpha.12 must still be surfaced.
        let releases = vec![release("v0.0.1-alpha.12", true, false)];
        let (_, prerelease) = select_updates("0.0.1-alpha.11-3-g05a127c", &releases);
        assert_eq!(prerelease.unwrap().tag, "v0.0.1-alpha.12");
    }

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
        assert!(platform_supports_self_update("windows", "aarch64"));
        assert!(platform_supports_self_update("macos", "x86_64"));
        assert!(platform_supports_self_update("macos", "aarch64"));
        assert!(!platform_supports_self_update("linux", "arm")); // 32-bit ARM: no release asset
        assert!(!platform_supports_self_update("freebsd", "x86_64"));
    }

    #[test]
    fn asset_filename_matches_release_ci_naming() {
        assert_eq!(asset_filename("v1.2.3", "linux", "x86_64").as_deref(), Some("recisdb-proxy-v1.2.3-linux-amd64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "linux", "aarch64").as_deref(), Some("recisdb-proxy-v1.2.3-linux-arm64.tar.gz"));
        assert_eq!(asset_filename("v1.2.3", "windows", "x86_64").as_deref(), Some("recisdb-v1.2.3-win-x64.zip"));
        assert_eq!(asset_filename("v1.2.3", "windows", "x86").as_deref(), Some("recisdb-v1.2.3-win-x86.zip"));
        assert_eq!(asset_filename("v1.2.3", "windows", "aarch64").as_deref(), Some("recisdb-v1.2.3-win-arm64.zip"));
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
