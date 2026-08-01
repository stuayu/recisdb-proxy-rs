//! recisdb-proxy: Network proxy server for BonDriver.
//!
//! This server allows BonDriver clients to connect over TCP
//! and access tuners remotely.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use log::{info, warn, error};

use recisdb_proxy::database;
use recisdb_proxy::logging;
use recisdb_proxy::alert;
use recisdb_proxy::epg_writer;
use recisdb_proxy::scheduler;
use recisdb_proxy::server;
use recisdb_proxy::tuner;
use recisdb_proxy::web;

use scheduler::{ScanScheduler, scan_scheduler::ScanSchedulerConfig};

use server::{Server, ServerConfig};
use tuner::TunerPoolConfig;

mod app_config;
mod service_cli;

/// recisdb-proxy - Network proxy server for BonDriver
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Optional subcommand. Without one, the server starts as before.
    #[command(subcommand)]
    command: Option<Command>,

    /// Address to listen on
    #[arg(short, long, default_value = "0.0.0.0:40070")]
    listen: SocketAddr,

    /// Address for web dashboard to listen on.
    ///
    /// Defaults to loopback-only (REVIEW_2026-07.md P0): the dashboard/API
    /// carries an auth token but is otherwise unencrypted (plain HTTP), so
    /// LAN exposure is opt-in. Pass `--web-listen 0.0.0.0:40080` (or set
    /// `[server] web_listen` in the TOML config) to expose it beyond
    /// localhost.
    #[arg(long, default_value = "127.0.0.1:40080")]
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

    /// Internal: set by the OS service manager when this process is launched
    /// as a registered service (`service/windows_scm.rs::launch_arguments`).
    /// On Windows this switches startup to the SCM dispatcher; on Unix it
    /// only marks the process (systemd/launchd exec the binary directly).
    #[arg(long, hide = true)]
    run_as_service: bool,

    /// Internal: service name to report to the Windows SCM dispatcher.
    #[arg(long, hide = true)]
    service_name: Option<String>,

    /// Internal: working directory to chdir into when launched as a service.
    /// The Windows SCM has no notion of a working directory, so the installer
    /// passes it explicitly.
    #[arg(long, hide = true)]
    service_workdir: Option<PathBuf>,
}

/// Subcommands. `None` (the default) keeps the historical behaviour of
/// starting the server directly.
#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// OSサービスとしての登録・制御 (install/uninstall/start/stop/restart/status)
    Service {
        #[command(subcommand)]
        action: service_cli::ServiceAction,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Subcommands run before any logging/database setup and never start the
    // server.
    if let Some(Command::Service { action }) = &args.command {
        let code = service_cli::run(action, args.config.as_deref());
        std::process::exit(code);
    }

    if args.run_as_service {
        recisdb_proxy::service::mark_running_as_service(args.service_name.as_deref());
        if let Some(dir) = &args.service_workdir {
            std::env::set_current_dir(dir).map_err(|e| {
                format!("failed to chdir into service working directory {dir:?}: {e}")
            })?;
        }
    }

    // Windows: a service process must talk to the SCM (report Running, honour
    // Stop) or the SCM reports "the service did not respond in a timely
    // fashion". `run_dispatcher` blocks until the service stops.
    #[cfg(windows)]
    if args.run_as_service {
        let name = args
            .service_name
            .clone()
            .unwrap_or_else(|| recisdb_proxy::service::DEFAULT_SERVICE_NAME.to_string());
        let args_for_body = args.clone();
        recisdb_proxy::service::windows_scm::run_dispatcher(&name, move |should_stop| {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("failed to build tokio runtime: {e}");
                    return;
                }
            };
            if let Err(e) = runtime.block_on(run_server(args_for_body, wait_stop_flag(should_stop)))
            {
                eprintln!("server exited with error: {e}");
            }
        })?;
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_server(args, shutdown_signal()))
}

/// Resolves when the process is asked to stop: Ctrl+C on every platform, plus
/// SIGTERM on Unix (what `systemctl stop` / `launchctl bootout` send — without
/// this the default disposition kills the process before anything can flush).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Windows service path: the SCM control handler runs on its own thread and
/// only flips an `AtomicBool`, so the async side polls it.
#[cfg(windows)]
async fn wait_stop_flag(flag: Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

async fn run_server(
    args: Args,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Load config file: explicit path > auto-detect > default
    let file_config = app_config::load_file_config(&args)?;

    // Merge logging configs (command line takes precedence)
    let (log_dir, log_retention_days, log_level) =
        app_config::resolve_log_settings(&args, &file_config);

    // Initialize logging with file output and rotation
    // Keep the returned guard alive for the whole program: dropping it stops
    // the background file-writer thread and flushes buffered log lines.
    let (_log_guard, log_buffer) =
        logging::init_logging(&log_dir, log_retention_days, args.verbose, log_level.as_deref())
            .expect("Failed to initialize logging");

    // Use log macros which are now bridged to tracing
    use log::{error, info};

    // Resolve everything else (listen addrs, tuner, TLS, web/mirakurun
    // toggles) from args × TOML config file (app_config.rs, M10).
    let resolved = app_config::load(&args, &file_config)?;
    let listen_addr = resolved.listen_addr;
    let web_listen_addr = resolved.web_listen_addr;
    let default_tuner = resolved.default_tuner;
    let max_connections = resolved.max_connections;
    let db_path = resolved.db_path;

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

    // tsreplace `command_path` is a trust boundary (REVIEW_2026-07.md S1):
    // the server executes it directly, so it can only be set here, from the
    // TOML config file, never from the Web API. If unset, whatever is
    // already in the DB (or the built-in default) is left untouched.
    if let Some(command_path) = &file_config.tsreplace.command_path {
        let db_guard = db.lock().await;
        match db_guard.set_tsreplace_command_path(command_path) {
            Ok(()) => info!("tsreplace command_path set from config file: {}", command_path),
            Err(e) => error!("Failed to set tsreplace command_path from config file: {}", e),
        }
    }
    // Same trust boundary as command_path (REVIEW S1): the optional stage-1
    // preprocessor executable is TOML-only too.
    if let Some(preprocessor_path) = &file_config.tsreplace.preprocessor_path {
        let db_guard = db.lock().await;
        match db_guard.set_tsreplace_preprocessor_path(preprocessor_path) {
            Ok(()) => info!("tsreplace preprocessor_path set from config file: {}", preprocessor_path),
            Err(e) => error!("Failed to set tsreplace preprocessor_path from config file: {}", e),
        }
    }

    // Browser-preview pipeline executable paths ([preview] section): same S1
    // trust boundary — TOML-only, never via the Web API.
    if let Some(command_path) = &file_config.preview.command_path {
        let db_guard = db.lock().await;
        match db_guard.set_preview_command_path(command_path) {
            Ok(()) => info!("preview command_path set from config file: {}", command_path),
            Err(e) => error!("Failed to set preview command_path from config file: {}", e),
        }
    }
    if let Some(preprocessor_path) = &file_config.preview.preprocessor_path {
        let db_guard = db.lock().await;
        match db_guard.set_preview_preprocessor_path(preprocessor_path) {
            Ok(()) => info!("preview preprocessor_path set from config file: {}", preprocessor_path),
            Err(e) => error!("Failed to set preview preprocessor_path from config file: {}", e),
        }
    }

    // Web API authentication (REVIEW_2026-07.md S2). Resolution order:
    // 1. TOML `[web] auth_token` (explicit override, persisted to DB too so
    //    the DB stays a consistent record of "what's currently valid").
    // 2. Token already persisted in the DB from a previous run.
    // 3. Newly generated token, persisted to the DB.
    // Whichever branch resolved it, the token is printed to the startup log
    // on every start (see below) so it can always be recovered from the
    // console/log file.
    let web_auth_enabled = resolved.web_auth_enabled;
    let web_auth_token = {
        let db_guard = db.lock().await;
        if let Some(token) = &file_config.web.auth_token {
            if let Err(e) = db_guard.set_web_auth_token(token) {
                warn!("Failed to persist web auth token override to database: {}", e);
            }
            token.clone()
        } else {
            match db_guard.get_web_auth_token() {
                Ok(Some(token)) => token,
                Ok(None) => {
                    let generated = web::auth::generate_token();
                    if let Err(e) = db_guard.set_web_auth_token(&generated) {
                        warn!("Failed to persist generated web auth token to database: {}", e);
                    }
                    info!("Generated a new Web API auth token (persisted to the database).");
                    generated
                }
                Err(e) => {
                    warn!("Failed to load web auth token from database: {}", e);
                    web::auth::generate_token()
                }
            }
        }
    };
    if web_auth_enabled {
        // Printed on EVERY start (not just when first generated) so users can
        // always recover the token from the startup log.
        info!("Web API auth token: {}", web_auth_token);
        info!("Enter this token in the dashboard (or send it as `Authorization: Bearer <token>`) to use the Web API.");
    } else {
        warn!("Web API authentication is DISABLED ([web] auth_enabled = false). Anyone who can reach the dashboard/API has full control. Use only on an isolated/trusted LAN.");
    }

    // Mirakurun-compatible API subset (STREAMING_DESIGN.md §7.1, P6):
    // opt-in, default disabled. The startup WARN for the unauthenticated
    // surface itself is logged from `web::start_web_server` once the router
    // is actually about to be nested in.
    let mirakurun_enabled = resolved.mirakurun_enabled;

    // TLS config, resolved from args × TOML by app_config::load above.
    #[cfg(feature = "tls")]
    let tls_config = resolved.tls_config;

    // Load tuner optimization config (incl. STREAMING_DESIGN.md §4/§9 P3
    // prefill/jitter settings) from database.
    //
    // `TunerPoolConfig` only carries the tuner-lifecycle fields (it is all
    // `TunerPool` itself uses); the prefill/jitter fields are surfaced via
    // `web::state::TunerConfigInfo` (dashboard display) and are otherwise
    // read directly from the DB per-session at StartStream time
    // (`Session::load_prefill_runtime_config`), not threaded through
    // `TunerPoolConfig`.
    let (tuner_config, prefill_config_for_web) = {
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
                prefill_view_ms,
                prefill_preview_ms,
                prefill_record_ms,
                jitter_safety_factor,
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
                info!(
                    "Loaded prefill config from database: view={}ms, preview={}ms, record={}ms, safety_factor={}",
                    prefill_view_ms, prefill_preview_ms, prefill_record_ms, jitter_safety_factor
                );
                (
                    TunerPoolConfig {
                        keep_alive_secs,
                        prewarm_enabled,
                        prewarm_timeout_secs,
                        set_channel_retry_interval_ms,
                        set_channel_retry_timeout_ms,
                        signal_poll_interval_ms,
                        signal_wait_timeout_ms,
                    },
                    (prefill_view_ms, prefill_preview_ms, prefill_record_ms, jitter_safety_factor),
                )
            }
            Err(e) => {
                warn!("Failed to load tuner config from database: {}", e);
                (TunerPoolConfig::default(), (1000, 2000, 6000, 1.5))
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
                        if let Err(e) = db_guard.request_immediate_scan(id) {
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

    // Start the EPG writer: batches EIT events collected by every live
    // tuner (`tuner/epg_collector.rs`) into the `programs` table. Must be
    // started before any tuner reader thread can run, since `EpgWriter::new`
    // installs the process-wide sender the collectors send into.
    let epg_db = db.clone();
    tokio::spawn(async move {
        let writer = epg_writer::EpgWriter::new(epg_db);
        writer.run().await;
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

    let (prefill_view_ms, prefill_preview_ms, prefill_record_ms, jitter_safety_factor) =
        prefill_config_for_web;
    let tuner_config_for_web = Some(web::state::TunerConfigInfo {
        keep_alive_secs: tuner_config.keep_alive_secs,
        prewarm_enabled: tuner_config.prewarm_enabled,
        prewarm_timeout_secs: tuner_config.prewarm_timeout_secs,
        set_channel_retry_interval_ms: tuner_config.set_channel_retry_interval_ms,
        set_channel_retry_timeout_ms: tuner_config.set_channel_retry_timeout_ms,
        signal_poll_interval_ms: tuner_config.signal_poll_interval_ms,
        signal_wait_timeout_ms: tuner_config.signal_wait_timeout_ms,
        prefill_view_ms,
        prefill_preview_ms,
        prefill_record_ms,
        jitter_safety_factor,
    });

    // Start web dashboard server
    let web_db = db.clone();
    let web_tuner_pool = Arc::clone(server.tuner_pool());
    // Same shared encoder pool every BNDP session uses (STREAMING_DESIGN.md
    // §5/§6 P4/P5), so an HTTP `?profile=preview` request and a TVTest
    // session watching the same channel join one running encoder chain.
    let web_encoder_pool = Arc::clone(server.encoder_pool());
    let web_session_registry = Arc::clone(&session_registry);
    let web_auth_config = web::auth::AuthConfig {
        enabled: web_auth_enabled,
        token: web_auth_token,
    };
    let web_log_buffer = Arc::clone(&log_buffer);
    let web_log_dir = log_dir.clone();
    tokio::spawn(async move {
        match web::start_web_server(
            web_listen_addr,
            web_db,
            web_tuner_pool,
            web_encoder_pool,
            web_session_registry,
            scan_config_for_web,
            tuner_config_for_web,
            web_auth_config,
            web_log_buffer,
            web_log_dir,
            mirakurun_enabled,
            Some(listen_addr),
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

    // Run until the listener exits or the caller's shutdown future resolves
    // (Ctrl+C/SIGTERM normally, the SCM stop flag when running as a Windows
    // service). Dropping the listener future stops new BNDP connections;
    // owned pools and database handles are then released in normal Rust drop
    // order.
    tokio::pin!(shutdown);
    tokio::select! {
        result = server.run() => result?,
        _ = &mut shutdown => info!("Shutdown signal received; stopping server"),
    }

    Ok(())
}
