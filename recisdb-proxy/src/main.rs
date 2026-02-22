//! recisdb-proxy: Network proxy server for BonDriver.
//!
//! This server allows BonDriver clients to connect over TCP
//! and access tuners remotely.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use std::sync::Arc;
use log::{info, warn, error};

mod bondriver;
mod database;
mod logging;
mod metrics;
mod alert;
mod scheduler;
mod server;
mod ts_analyzer;
mod tuner;
mod aribb24;
mod web;

use scheduler::{ScanScheduler, scan_scheduler::ScanSchedulerConfig};

use server::{Server, ServerConfig};
use tuner::TunerPoolConfig;

/// recisdb-proxy - Network proxy server for BonDriver
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Address to listen on
    #[arg(short, long, default_value = "0.0.0.0:12345")]
    listen: SocketAddr,

    /// Address for web dashboard to listen on
    #[arg(long, default_value = "0.0.0.0:8080")]
    web_listen: SocketAddr,

    /// Path to the default tuner device
    #[arg(short, long)]
    tuner: Option<String>,

    /// Path to the database file
    #[arg(short, long, default_value = "recisdb-proxy.db")]
    database: PathBuf,

    /// Maximum concurrent connections
    #[arg(short = 'c', long, default_value = "64")]
    max_connections: usize,

    /// Configuration file path
    #[arg(short = 'f', long)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Enable automatic channel scanning
    #[arg(long, default_value = "true")]
    enable_scan: bool,

    /// Trigger a channel scan on server startup
    #[arg(long)]
    scan_on_start: bool,

    /// Channel scan check interval in seconds
    #[arg(long, default_value = "60")]
    scan_interval: u64,

    /// Maximum concurrent channel scans
    #[arg(long, default_value = "1")]
    max_concurrent_scans: usize,

    /// Directory where log files are stored
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    /// Number of days to keep log files
    #[arg(long, default_value = "7")]
    log_retention_days: u64,

    /// Enable TLS (requires tls feature)
    #[cfg(feature = "tls")]
    #[arg(long)]
    tls: bool,

    /// Path to CA certificate (for TLS)
    #[cfg(feature = "tls")]
    #[arg(long)]
    ca_cert: Option<PathBuf>,

    /// Path to server certificate (for TLS)
    #[cfg(feature = "tls")]
    #[arg(long)]
    server_cert: Option<PathBuf>,

    /// Path to server key (for TLS)
    #[cfg(feature = "tls")]
    #[arg(long)]
    server_key: Option<PathBuf>,
}

/// Configuration file format.
#[derive(Debug, serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    database: DatabaseSection,
    #[serde(default)]
    logging: LoggingSection,
    #[cfg(feature = "tls")]
    #[serde(default)]
    tls: TlsSection,
}

#[derive(Debug, serde::Deserialize, Default)]
struct ServerSection {
    listen: Option<String>,
    web_listen: Option<String>,
    tuner: Option<String>,
    max_connections: Option<usize>,
}

#[derive(Debug, serde::Deserialize, Default)]
struct LoggingSection {
    log_dir: Option<String>,
    retention_days: Option<u64>,
    level: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
struct DatabaseSection {
    path: Option<String>,
}

#[cfg(feature = "tls")]
#[derive(Debug, serde::Deserialize, Default)]
struct TlsSection {
    enabled: Option<bool>,
    ca_cert: Option<String>,
    server_cert: Option<String>,
    server_key: Option<String>,
    require_client_cert: Option<bool>,
}

fn load_config(path: &PathBuf) -> Result<ConfigFile, Box<dyn std::error::Error>> {
    let contents = std::fs::read_to_string(path)?;
    let config: ConfigFile = toml::from_str(&contents)?;
    Ok(config)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let args = Args::parse();

    // Load config file: explicit path > auto-detect > default
    let config_path = args.config.clone().or_else(|| {
        let default_path = PathBuf::from("recisdb-proxy.toml");
        if default_path.exists() {
            Some(default_path)
        } else {
            None
        }
    });
    let file_config = if let Some(config_path) = &config_path {
        match load_config(config_path) {
            Ok(c) => {
                eprintln!("Loaded config from: {}", config_path.display());
                c
            }
            Err(e) => {
                eprintln!("Failed to load config file: {}", e);
                return Err(e);
            }
        }
    } else {
        ConfigFile::default()
    };

    // Merge logging configs (command line takes precedence)
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

    // Initialize logging with file output and rotation
    let log_level = file_config.logging.level.as_deref();
    logging::init_logging(&log_dir, log_retention_days, args.verbose, log_level)
        .expect("Failed to initialize logging");

    // Use log macros which are now bridged to tracing
    use log::{error, info};

    // Get database path and other settings from config
    let listen_addr = args.listen;
    let web_listen_addr = if let Ok(addr) = file_config.server.web_listen.as_ref().unwrap_or(&"0.0.0.0:8080".to_string()).parse::<SocketAddr>() {
        addr
    } else {
        "0.0.0.0:8080".parse::<SocketAddr>()?
    };
    let default_tuner = args.tuner.or(file_config.server.tuner);
    let max_connections = file_config
        .server
        .max_connections
        .unwrap_or(args.max_connections);
    let db_path = file_config
        .database
        .path
        .map(PathBuf::from)
        .unwrap_or(args.database);

    // Initialize database
    info!("Opening database: {:?}", db_path);
    let db = match database::Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return Err(e.into());
        }
    };
    let db = std::sync::Arc::new(tokio::sync::Mutex::new(db));

    // Build TLS config if enabled
    #[cfg(feature = "tls")]
    let tls_config = if args.tls {
        // Get TLS paths from args or config file
        let ca_cert = args
            .ca_cert
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| file_config.tls.ca_cert.clone());
        let server_cert = args
            .server_cert
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| file_config.tls.server_cert.clone());
        let server_key = args
            .server_key
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
                Some(server::TlsConfig {
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
                Some(server::TlsConfig {
                    ca_cert_path: ca,
                    server_cert_path: cert,
                    server_key_path: key,
                    require_client_cert,
                })
            })
    };

    // Load tuner optimization config from database
    let tuner_config = {
        let db_lock = db.lock().await;
        match db_lock.get_tuner_config() {
            Ok((
                keep_alive_secs,
                prewarm_enabled,
                prewarm_timeout_secs,
                set_channel_retry_interval_ms,
                set_channel_retry_timeout_ms,
                signal_poll_interval_ms,
                signal_wait_timeout_ms,
            )) => {
                info!(
                    "Loaded tuner config from database: keep_alive={}s, prewarm_enabled={}, prewarm_timeout={}s, set_retry_interval={}ms, set_retry_timeout={}ms, signal_poll={}ms, signal_wait_timeout={}ms",
                    keep_alive_secs,
                    prewarm_enabled,
                    prewarm_timeout_secs,
                    set_channel_retry_interval_ms,
                    set_channel_retry_timeout_ms,
                    signal_poll_interval_ms,
                    signal_wait_timeout_ms
                );
                TunerPoolConfig {
                    keep_alive_secs,
                    prewarm_enabled,
                    prewarm_timeout_secs,
                    set_channel_retry_interval_ms,
                    set_channel_retry_timeout_ms,
                    signal_poll_interval_ms,
                    signal_wait_timeout_ms,
                }
            }
            Err(e) => {
                warn!("Failed to load tuner config from database: {}", e);
                TunerPoolConfig::default()
            }
        }
    };

    // Build server config
    let config = ServerConfig {
        listen_addr,
        max_connections,
        default_tuner: default_tuner.clone(),
        database: db.clone(),
        tuner_config: tuner_config.clone(),
        #[cfg(feature = "tls")]
        tls_config,
    };

    info!("recisdb-proxy starting...");
    info!("  Listen address: {}", config.listen_addr);
    info!("  Max connections: {}", config.max_connections);
    info!("  Database: {:?}", db_path);
    if let Some(tuner) = &config.default_tuner {
        info!("  Default tuner: {}", tuner);

        // Register default tuner in database for scanning
        {
            let db_guard = db.lock().await;
            match db_guard.get_or_create_bon_driver(tuner) {
                Ok(id) => {
                    info!("  Registered tuner in database (id={})", id);

                    // If scan-on-start is requested, enable immediate scan for this driver
                    if args.scan_on_start {
                        if let Err(e) = db_guard.enable_immediate_scan(id) {
                            error!("Failed to enable immediate scan: {}", e);
                        } else {
                            info!("  Enabled immediate scan for tuner (id={})", id);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to register tuner in database: {}", e);
                }
            }
        }
    }

    // Create session registry for tracking active sessions
    let session_registry = Arc::new(web::SessionRegistry::new());

    // Start alert manager
    let alert_db = db.clone();
    let alert_registry = Arc::clone(&session_registry);
    tokio::spawn(async move {
        let manager = alert::AlertManager::new(alert_db, alert_registry);
        manager.run().await;
    });

    // Create server
    let server = Server::new(config, Arc::clone(&session_registry));

    // Prepare scan configuration to share with web server
    let scan_config_for_web = if args.enable_scan {
        Some(web::state::ScanSchedulerInfo {
            check_interval_secs: args.scan_interval,
            max_concurrent_scans: args.max_concurrent_scans,
            scan_timeout_secs: 900, // From ScanSchedulerConfig default
            signal_lock_wait_ms: 500,
            ts_read_timeout_ms: 300000,
        })
    } else {
        None
    };

    let tuner_config_for_web = Some(web::state::TunerConfigInfo {
        keep_alive_secs: tuner_config.keep_alive_secs,
        prewarm_enabled: tuner_config.prewarm_enabled,
        prewarm_timeout_secs: tuner_config.prewarm_timeout_secs,
        set_channel_retry_interval_ms: tuner_config.set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms: tuner_config.set_channel_retry_timeout_ms,
        signal_poll_interval_ms: tuner_config.signal_poll_interval_ms,
        signal_wait_timeout_ms: tuner_config.signal_wait_timeout_ms,
    });

    // Start web dashboard server
    let web_db = db.clone();
    let web_tuner_pool = Arc::clone(server.tuner_pool());
    let web_session_registry = Arc::clone(&session_registry);
    tokio::spawn(async move {
        match web::start_web_server(
            web_listen_addr,
            web_db,
            web_tuner_pool,
            web_session_registry,
            scan_config_for_web,
            tuner_config_for_web,
        ).await {
            Ok(_) => info!("Web dashboard server stopped"),
            Err(e) => error!("Web dashboard error: {}", e),
        }
    });

    info!("Web dashboard listening on http://{}", web_listen_addr);

    // Load scan scheduler configuration from database
    let (db_check_interval, db_max_concurrent, db_timeout, db_signal_lock_wait_ms, db_ts_read_timeout_ms) = {
        let db_lock = db.lock().await;
        match db_lock.get_scan_scheduler_config() {
            Ok(config) => {
                info!(
                    "Loaded scan scheduler config from database: interval={}s, concurrent={}, timeout={}s, signal_lock_wait={}ms, ts_read_timeout={}ms",
                    config.0,
                    config.1,
                    config.2,
                    config.3,
                    config.4
                );
                config
            }
            Err(e) => {
                warn!("Failed to load scan scheduler config from database: {}", e);
                (args.scan_interval, args.max_concurrent_scans, 900, 500, 300000)
            }
        }
    };

    // Start scan scheduler if enabled
    if args.enable_scan {
        let scan_config = ScanSchedulerConfig {
            check_interval_secs: db_check_interval,
            max_concurrent_scans: db_max_concurrent,
            scan_timeout_secs: db_timeout,
            signal_lock_wait_ms: db_signal_lock_wait_ms,
            ts_read_timeout_ms: db_ts_read_timeout_ms,
        };

        let scheduler = Arc::new(ScanScheduler::new(
            db.clone(),
            Arc::clone(server.tuner_pool()),
            scan_config,
        ));

        info!("Starting channel scan scheduler (interval: {}s, max concurrent: {})", 
              db_check_interval, db_max_concurrent);
        let _scheduler_handle = Arc::clone(&scheduler).start();

        // Trigger immediate scan if requested
        if args.scan_on_start {
            info!("Triggering initial channel scan...");
            let scheduler_for_scan = Arc::clone(&scheduler);
            tokio::spawn(async move {
                // Wait a moment for the scheduler to initialize
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                if let Err(e) = scheduler_for_scan.trigger_scan().await {
                    error!("Initial scan failed: {}", e);
                }
            });
        }
    }

    // Run server
    server.run().await?;

    Ok(())
}
