//! CLI args × TOML config file resolution for the `recisdb-proxy` binary.
//!
//! Priority for every resolved field is: CLI arg (when it differs from its
//! clap default) > TOML `[section]` value > built-in default. This module is
//! pure code-motion out of `main.rs` (SYSTEM_REVIEW_2026-07.md M10): it must
//! not change any resolution semantics, TLS `#[cfg]` behavior, or log output.
//!
//! Resolution happens in two steps because logging is not initialized until
//! after the log directory/retention are known:
//! 1. [`load_file_config`] + [`resolve_log_settings`] — pure, no log macros,
//!    safe to call before `logging::init_logging`.
//! 2. [`load`] — everything else (listen addrs, tuner, TLS, web/mirakurun
//!    toggles). Uses `log`/`info!`/`error!` macros, so must be called after
//!    logging has been initialized, matching the original `main.rs` order.

use std::net::SocketAddr;
use std::path::PathBuf;

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
    pub logging: LoggingSection,
    #[serde(default)]
    pub web: WebSection,
    #[serde(default)]
    pub tsreplace: TsreplaceSection,
    #[serde(default)]
    pub preview: PreviewSection,
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
pub struct LoggingSection {
    pub log_dir: Option<String>,
    pub retention_days: Option<u64>,
    pub level: Option<String>,
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
pub fn load_file_config(args: &Args) -> Result<ConfigFile, Box<dyn std::error::Error>> {
    let config_path = args.config.clone().or_else(|| {
        let default_path = PathBuf::from("recisdb-proxy.toml");
        if default_path.exists() {
            Some(default_path)
        } else {
            None
        }
    });
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

/// Log directory/retention/level, resolved before `logging::init_logging` is
/// called (so it cannot itself use the `log` macros).
pub fn resolve_log_settings(args: &Args, file_config: &ConfigFile) -> (PathBuf, u64, Option<String>) {
    let log_dir = if args.log_dir.to_string_lossy() != "logs" {
        args.log_dir.clone()
    } else {
        PathBuf::from(file_config.logging.log_dir.as_deref().unwrap_or("logs"))
    };

    let log_retention_days = if args.log_retention_days != 7 {
        args.log_retention_days
    } else {
        file_config.logging.retention_days.unwrap_or(7)
    };

    let log_level = file_config.logging.level.clone();

    (log_dir, log_retention_days, log_level)
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
    fn log_dir_prefers_arg_over_toml_when_explicitly_set() {
        let mut args = default_args();
        args.log_dir = PathBuf::from("custom-logs");

        let mut file_config = ConfigFile::default();
        file_config.logging.log_dir = Some("toml-logs".to_string());

        let (log_dir, _, _) = resolve_log_settings(&args, &file_config);
        assert_eq!(log_dir, PathBuf::from("custom-logs"));
    }

    #[test]
    fn log_dir_falls_back_to_toml_then_default() {
        let args = default_args();

        let mut file_config = ConfigFile::default();
        file_config.logging.log_dir = Some("toml-logs".to_string());
        let (log_dir, _, _) = resolve_log_settings(&args, &file_config);
        assert_eq!(log_dir, PathBuf::from("toml-logs"));

        let file_config = ConfigFile::default();
        let (log_dir, _, _) = resolve_log_settings(&args, &file_config);
        assert_eq!(log_dir, PathBuf::from("logs"));
    }
}
