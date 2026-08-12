//! CLI args × TOML config file resolution for the `recisdb-proxy` binary.
//!
//! Priority for every resolved field is: CLI arg (when it differs from its
//! clap default) > TOML `[section]` value > built-in default. This module is
//! pure code-motion out of `main.rs` (SYSTEM_REVIEW_2026-07.md M10): it must
//! not change any resolution semantics, TLS `#[cfg]` behavior, or log output.
//!
//! Resolution happens in two steps because logging is not initialized until
//! after the log directory/retention are known:
//! 1. [`load_file_config`] + [`resolve_log_dir`] — pure, no log macros,
//!    safe to call before `logging::init_logging`.
//! 2. [`load`] — everything else (listen addrs, tuner, TLS, web/mirakurun
//!    toggles). Uses `log`/`info!`/`error!` macros, so must be called after
//!    logging has been initialized, matching the original `main.rs` order.

use std::net::SocketAddr;
use std::path::PathBuf;

use log::warn;
#[cfg(feature = "tls")]
use log::{error, info};

use crate::Args;

/// Configuration file format.
#[derive(Debug, serde::Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub database: DatabaseSection,
    #[serde(default)]
    pub web: WebSection,
    #[serde(default)]
    pub tsreplace: TsreplaceSection,
    #[serde(default)]
    pub preview: PreviewSection,
    #[serde(default)]
    pub mmttlv: MmtTlvSection,
    #[serde(default)]
    pub mirakurun: MirakurunSection,
    #[cfg(feature = "tls")]
    #[serde(default)]
    pub tls: TlsSection,
}

/// Web dashboard/API configuration (REVIEW_2026-07.md S2).
#[derive(Debug, serde::Deserialize, Default)]
pub struct WebSection {
    /// Require `Authorization: Bearer <token>` on all `/api/*` requests.
    /// Defaults to `true`; set to `false` only for isolated LAN testing.
    pub auth_enabled: Option<bool>,
    /// Explicit bearer token. If unset, a token is generated once and
    /// persisted to the database. Whatever token is in effect is printed
    /// to the startup log on every start (when auth is enabled).
    pub auth_token: Option<String>,
}

/// Mirakurun-compatible API subset configuration
/// (STREAMING_DESIGN.md §7.1, P6).
#[derive(Debug, serde::Deserialize, Default)]
pub struct MirakurunSection {
    /// Mount the unauthenticated `/mirakurun/api/*` router
    /// (`web/mirakurun.rs`). Defaults to `false`: this endpoint carries no
    /// bearer-token auth at all (real Mirakurun clients — EPGStation/mirakc/
    /// KonomiTV — send none), so it is opt-in even though `web_listen`
    /// already defaults to loopback-only.
    pub enabled: Option<bool>,
    /// Prefecture name (e.g. `"福島"`) whose terrestrial stations are the
    /// *local* ones, reported as Mirakurun channel type `GR`. Every other
    /// terrestrial region is then reported as `NW1`..`NW40`
    /// (`web/mirakurun.rs::terrestrial_type_map`), which is how EPGStation's
    /// fork separates out-of-area stations into their own programme-guide
    /// tabs.
    ///
    /// Unset (the default) keeps every terrestrial station on `GR`, matching
    /// the behaviour from before this option existed — correct for a
    /// single-area install, and unhelpful only when several areas are
    /// received at once.
    ///
    /// Accepts a prefecture name as spelled by
    /// `recisdb_protocol::broadcast_region`; the wide-area Kanto network is
    /// `"東京"`.
    pub home_region: Option<String>,
}

/// tsreplace (external encoder) configuration that must only be settable via
/// the config file (REVIEW_2026-07.md S1 — see
/// `Database::set_tsreplace_command_path` for why).
#[derive(Debug, serde::Deserialize, Default)]
pub struct TsreplaceSection {
    /// Path to the tsreplace (or compatible) executable. This is
    /// intentionally not exposed via the Web API: the server executes this
    /// path directly (`Command::new(command_path)`), so allowing it to be
    /// changed by anyone who can reach the dashboard would be a remote code
    /// execution vector.
    pub command_path: Option<String>,
    /// Optional stage-1 (preprocessor) executable, e.g. tsreadex, piped in
    /// front of `command_path`: `TS -> preprocessor -> encoder -> stdout`.
    /// Same trust boundary as `command_path` (TOML-only, never via the Web
    /// API). Set to an empty string to clear an already-persisted value.
    pub preprocessor_path: Option<String>,
}

/// MMT/TLV → MPEG-2 TS converter (dantto4k) configuration for 4K tuners.
///
/// `command_path` is TOML-only for the same reason as `[tsreplace]`: the
/// server executes it directly, so exposing it to the Web API would be a
/// remote code execution vector. The CAS settings live here too rather than in
/// the database, because without one of them the converter silently produces
/// an undecipherable TS — they are part of getting the driver working at all,
/// not a per-session preference.
#[derive(Debug, serde::Deserialize, Default)]
pub struct MmtTlvSection {
    /// Path to the `dantto4k` executable.
    pub command_path: Option<String>,
    /// `--casProxyServer`: address of a running CasProxyServer, e.g.
    /// `127.0.0.1:24000`. The converter falls back to local PC/SC when it
    /// cannot reach this, so a wrong address looks like "no card reader".
    pub cas_proxy_server: Option<String>,
    /// `--smartCardReaderName`: PC/SC reader holding the A-CAS card.
    /// Use `dantto4k --listSmartCardReader` to get the exact name.
    pub smart_card_reader_name: Option<String>,
    /// `--frontend-descrambled`: only remux, assume something upstream already
    /// descrambled. Leave this off when reading raw off a tuner — nothing has
    /// descrambled at that point, and turning it on yields a full-size,
    /// unplayable TS with no warning.
    #[serde(default)]
    pub frontend_descrambled: bool,
    /// Extra arguments appended verbatim (`--disableADTSConversion`,
    /// `--customWinscardDLL <path>`, …).
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// Browser-preview (`?profile=preview`) encoder configuration that must only
/// be settable via the config file (REVIEW_2026-07.md S1). Fully separate
/// from `[tsreplace]`, which configures the BNDP (TVTest) session pipeline.
#[derive(Debug, serde::Deserialize, Default)]
pub struct PreviewSection {
    /// Path to the preview encoder executable (e.g. QSVEncC). TOML-only for
    /// the same RCE-prevention reason as `[tsreplace] command_path`.
    pub command_path: Option<String>,
    /// Optional stage-1 (preprocessor) executable, e.g. tsreadex. TOML-only.
    /// Set to an empty string to clear an already-persisted value.
    pub preprocessor_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ServerSection {
    pub listen: Option<String>,
    pub web_listen: Option<String>,
    pub tuner: Option<String>,
    pub max_connections: Option<usize>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct DatabaseSection {
    pub path: Option<String>,
}

#[cfg(feature = "tls")]
#[derive(Debug, serde::Deserialize, Default)]
pub struct TlsSection {
    pub enabled: Option<bool>,
    pub ca_cert: Option<String>,
    pub server_cert: Option<String>,
    pub server_key: Option<String>,
    pub require_client_cert: Option<bool>,
}

fn parse_config_file(path: &PathBuf) -> Result<ConfigFile, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let config: ConfigFile = toml::from_str(&contents)?;
    Ok(config)
}

/// Resolves which config file (if any) to load — explicit `--config` path,
/// else an auto-detected `recisdb-proxy.toml` in the working directory —
/// and loads it. Returns `ConfigFile::default()` if neither is present.
///
/// Uses `eprintln!` rather than the `log` macros: this runs before
/// `logging::init_logging`, matching the original `main.rs` order.
/// 実際に読み込む設定ファイルのパス。`-f` があればそれ、無ければ作業フォルダの
/// `recisdb-proxy.toml` (存在する場合のみ)。
///
/// プレビュー自動セットアップ (`preview_setup`) が `[preview]` セクションを
/// 書き戻す先を知る必要があるため、`load_file_config` から切り出して公開している。
/// 書き戻さないと、起動時に TOML が DB を上書きする経路 (main.rs) で設定が
/// 巻き戻ってしまう。
pub fn resolve_config_path(args: &Args) -> Option<PathBuf> {
    args.config.clone().or_else(|| {
        let default_path = PathBuf::from("recisdb-proxy.toml");
        if default_path.exists() {
            Some(default_path)
        } else {
            None
        }
    })
}

pub fn load_file_config(args: &Args) -> Result<ConfigFile, Box<dyn std::error::Error>> {
    let config_path = resolve_config_path(args);
    if let Some(config_path) = &config_path {
        match parse_config_file(config_path) {
            Ok(c) => {
                eprintln!("Loaded config from: {}", config_path.display());
                Ok(c)
            }
            Err(e) => {
                eprintln!("Failed to load config file: {}", e);
                Err(e)
            }
        }
    } else {
        Ok(ConfigFile::default())
    }
}

/// Log output directory, resolved before `logging::init_logging` is called
/// (so it cannot itself use the `log` macros).
///
/// Unlike the rest of `[logging]` (level, retention), this stays CLI-only —
/// there is no TOML fallback anymore: `--log-dir` (default `"logs"`) is the
/// only source. Log **level** and **retention** are no longer read from the
/// TOML file at all (the `[logging]` section is gone); they live in the
/// database's `log_config` table and are managed from the Web dashboard
/// ("設定 > ログ出力"), applied by `main.rs` once the database is open. A
/// stray `[logging]` section left over in an old `recisdb-proxy.toml` is
/// harmless — serde silently ignores unknown TOML sections.
pub fn resolve_log_dir(args: &Args) -> PathBuf {
    args.log_dir.clone()
}

/// Everything resolved by [`load`] besides logging (which must be up before
/// `load` runs, since TLS resolution logs via `info!`/`error!`).
pub struct ResolvedConfig {
    pub listen_addr: SocketAddr,
    pub web_listen_addr: SocketAddr,
    pub default_tuner: Option<String>,
    pub max_connections: usize,
    pub db_path: PathBuf,
    pub web_auth_enabled: bool,
    pub mirakurun_enabled: bool,
    /// Region IDs resolved from `[mirakurun] home_region`, empty when unset
    /// (or set to a name no prefecture matches — that case warns and falls
    /// back to "every terrestrial station is GR" rather than failing startup
    /// over a display-level setting). One name can cover several IDs; see
    /// `recisdb_protocol::broadcast_region::region_ids_from_prefecture_name`.
    pub mirakurun_home_regions: Vec<u8>,
    #[cfg(feature = "tls")]
    pub tls_config: Option<recisdb_proxy::server::TlsConfig>,
}

/// Resolves CLI args against the TOML file for everything except logging.
/// Must be called after `logging::init_logging` (TLS resolution logs via
/// `info!`/`error!`, matching the original `main.rs` behavior/ordering).
pub fn load(args: &Args, file_config: &ConfigFile) -> Result<ResolvedConfig, Box<dyn std::error::Error>> {
    let listen_addr = if let Some(addr_str) = &file_config.server.listen {
        addr_str.parse::<SocketAddr>().unwrap_or(args.listen)
    } else {
        args.listen
    };
    let web_listen_addr = if let Some(addr_str) = &file_config.server.web_listen {
        addr_str.parse::<SocketAddr>().unwrap_or(args.web_listen)
    } else {
        args.web_listen
    };
    let default_tuner = args.tuner.clone().or_else(|| file_config.server.tuner.clone());
    let max_connections = file_config
        .server
        .max_connections
        .unwrap_or(args.max_connections);
    let db_path = file_config
        .database
        .path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| args.database.clone());

    let web_auth_enabled = file_config.web.auth_enabled.unwrap_or(true);
    let mirakurun_enabled = file_config.mirakurun.enabled.unwrap_or(false);
    let mirakurun_home_regions = match file_config.mirakurun.home_region.as_deref() {
        None => Vec::new(),
        Some(name) => {
            let resolved = recisdb_protocol::broadcast_region::region_ids_from_prefecture_name(name);
            if resolved.is_empty() {
                warn!(
                    "[mirakurun] home_region = \"{}\" matches no prefecture; every terrestrial \
                     station will be reported as GR",
                    name
                );
            }
            resolved
        }
    };

    // Build TLS config if enabled
    #[cfg(feature = "tls")]
    let tls_config = if args.tls {
        // Get TLS paths from args or config file
        let ca_cert = args
            .ca_cert
            .clone()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| file_config.tls.ca_cert.clone());
        let server_cert = args
            .server_cert
            .clone()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| file_config.tls.server_cert.clone());
        let server_key = args
            .server_key
            .clone()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| file_config.tls.server_key.clone());
        let require_client_cert = file_config.tls.require_client_cert.unwrap_or(false);

        match (ca_cert, server_cert, server_key) {
            (Some(ca), Some(cert), Some(key)) => {
                info!("TLS enabled with:");
                info!("  CA certificate: {}", ca);
                info!("  Server certificate: {}", cert);
                info!("  Server key: {}", key);
                info!("  Require client cert: {}", require_client_cert);
                Some(recisdb_proxy::server::TlsConfig {
                    ca_cert_path: ca,
                    server_cert_path: cert,
                    server_key_path: key,
                    require_client_cert,
                })
            }
            _ => {
                error!("TLS enabled but missing certificate/key paths");
                error!("Required: --ca-cert, --server-cert, --server-key");
                return Err("TLS configuration incomplete".into());
            }
        }
    } else {
        file_config
            .tls
            .enabled
            .filter(|&e| e)
            .and_then(|_| {
                let ca = file_config.tls.ca_cert.clone()?;
                let cert = file_config.tls.server_cert.clone()?;
                let key = file_config.tls.server_key.clone()?;
                let require_client_cert = file_config.tls.require_client_cert.unwrap_or(false);
                info!("TLS enabled from config file");
                Some(recisdb_proxy::server::TlsConfig {
                    ca_cert_path: ca,
                    server_cert_path: cert,
                    server_key_path: key,
                    require_client_cert,
                })
            })
    };

    Ok(ResolvedConfig {
        listen_addr,
        web_listen_addr,
        default_tuner,
        max_connections,
        db_path,
        web_auth_enabled,
        mirakurun_enabled,
        mirakurun_home_regions,
        #[cfg(feature = "tls")]
        tls_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Builds a default `Args` as clap would (all clap defaults, no CLI
    /// overrides), so tests can flip individual fields to exercise priority.
    fn default_args() -> Args {
        Args::parse_from(["recisdb-proxy"])
    }

    #[test]
    fn max_connections_prefers_toml_over_arg_default() {
        let args = default_args();
        assert_eq!(args.max_connections, 64);

        let mut file_config = ConfigFile::default();
        file_config.server.max_connections = Some(128);

        let resolved = load(&args, &file_config).expect("load should succeed");
        assert_eq!(resolved.max_connections, 128);
    }

    #[test]
    fn max_connections_prefers_arg_over_toml_when_both_set() {
        // Current semantics (main.rs pre-move): TOML wins whenever it is
        // present at all, regardless of whether the CLI value was explicitly
        // passed or is just its clap default. This test locks in that exact
        // (pre-existing) behavior so the code motion doesn't silently change
        // it.
        let mut args = default_args();
        args.max_connections = 32;

        let mut file_config = ConfigFile::default();
        file_config.server.max_connections = Some(128);

        let resolved = load(&args, &file_config).expect("load should succeed");
        assert_eq!(resolved.max_connections, 128);
    }

    #[test]
    fn max_connections_falls_back_to_arg_default_without_toml() {
        let args = default_args();
        let file_config = ConfigFile::default();

        let resolved = load(&args, &file_config).expect("load should succeed");
        assert_eq!(resolved.max_connections, args.max_connections);
    }

    #[test]
    fn default_tuner_prefers_arg_over_toml() {
        let mut args = default_args();
        args.tuner = Some("arg-tuner".to_string());

        let mut file_config = ConfigFile::default();
        file_config.server.tuner = Some("toml-tuner".to_string());

        let resolved = load(&args, &file_config).expect("load should succeed");
        assert_eq!(resolved.default_tuner.as_deref(), Some("arg-tuner"));
    }

    #[test]
    fn log_dir_uses_the_cli_arg() {
        let mut args = default_args();
        args.log_dir = PathBuf::from("custom-logs");
        assert_eq!(resolve_log_dir(&args), PathBuf::from("custom-logs"));
    }

    #[test]
    fn log_dir_falls_back_to_default_without_arg() {
        let args = default_args();
        assert_eq!(resolve_log_dir(&args), PathBuf::from("logs"));
    }
}
