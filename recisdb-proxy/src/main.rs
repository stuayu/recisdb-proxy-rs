//! recisdb-proxy: Network proxy server for BonDriver.
//!
//! This server allows BonDriver clients to connect over TCP
//! and access tuners remotely.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use log::{error, info, warn};
use std::sync::Arc;

use recisdb_proxy::alert;
use recisdb_proxy::database;
use recisdb_proxy::epg_writer;
use recisdb_proxy::logging;
use recisdb_proxy::nit_writer;
use recisdb_proxy::scheduler;
use recisdb_proxy::server;
use recisdb_proxy::tuner;
use recisdb_proxy::web;

use scheduler::{scan_scheduler::ScanSchedulerConfig, ScanScheduler};

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
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
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
    let config_path_for_web = app_config::resolve_config_path(&args);
    let file_config = app_config::load_file_config(&args)?;

    // Log output directory: CLI-only (`--log-dir`), no TOML fallback.
    let log_dir = app_config::resolve_log_dir(&args);

    // Initialize logging with file output and rotation. The initial level is
    // RUST_LOG > --verbose > "info"; the DB-configured level (`log_config`
    // table) is applied right after the database opens, below. The initial
    // cleanup pass uses `args.log_retention_days` (default 7) since the DB
    // is not open yet — the DB's `retention_days` governs every cleanup
    // after that.
    //
    // Keep the returned guard alive for the whole program: dropping it stops
    // the background file-writer thread and flushes buffered log lines.
    let (_log_guard, log_buffer, log_level_handle) =
        logging::init_logging(&log_dir, args.log_retention_days, args.verbose)
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

    // Apply the DB-configured log level/retention (moved off TOML — see
    // `logging.rs` module doc). This is the point where `log_config`
    // (database) becomes the source of truth for the level: whatever
    // `--verbose` picked at `init_logging` time was only ever a placeholder
    // until the database was available to read. RUST_LOG is the one exception
    // — see below.
    {
        let (db_level, db_retention_days) = {
            let db_guard = db.lock().await;
            match db_guard.get_log_config() {
                Ok(config) => config,
                Err(e) => {
                    warn!(
                        "Failed to load log config from database, keeping startup level: {}",
                        e
                    );
                    (log_level_handle.current(), args.log_retention_days)
                }
            }
        };
        // RUST_LOG wins at startup: it is the debugging escape hatch, and it
        // can carry per-module directives that a single DB level cannot
        // express — `set_level` replaces the *whole* filter, so applying the
        // DB level here would silently throw those directives away. A later
        // change from the dashboard still overrides it (documented in
        // `LogLevelHandle::set_level`).
        if log_level_handle.env_override() {
            info!(
                "RUST_LOG is set, so it keeps precedence over the database log level '{}' for this run. Changing the level from the Web dashboard still overrides RUST_LOG entirely.",
                db_level
            );
        } else {
            match log_level_handle.set_level(&db_level) {
                Ok(()) => info!("Log level set from database configuration: {}", db_level),
                Err(e) => warn!("Failed to apply database log level '{}': {}", db_level, e),
            }
        }
        // Re-run the cleanup pass with the DB's retention (the one at
        // `init_logging` time used `args.log_retention_days`, since the DB
        // was not open yet).
        if let Err(e) = logging::rotate_logs(&log_dir, db_retention_days) {
            warn!("Failed to rotate logs with database retention_days: {}", e);
        }
    }

    // tsreplace `command_path` is a trust boundary (REVIEW_2026-07.md S1):
    // the server executes it directly, so it can only be set here, from the
    // TOML config file, never from the Web API. If unset, whatever is
    // already in the DB (or the built-in default) is left untouched.
    if let Some(command_path) = &file_config.tsreplace.command_path {
        let db_guard = db.lock().await;
        match db_guard.set_tsreplace_command_path(command_path) {
            Ok(()) => info!(
                "tsreplace command_path set from config file: {}",
                command_path
            ),
            Err(e) => error!(
                "Failed to set tsreplace command_path from config file: {}",
                e
            ),
        }
    }
    // Same trust boundary as command_path (REVIEW S1): the optional stage-1
    // preprocessor executable is TOML-only too.
    if let Some(preprocessor_path) = &file_config.tsreplace.preprocessor_path {
        let db_guard = db.lock().await;
        match db_guard.set_tsreplace_preprocessor_path(preprocessor_path) {
            Ok(()) => info!(
                "tsreplace preprocessor_path set from config file: {}",
                preprocessor_path
            ),
            Err(e) => error!(
                "Failed to set tsreplace preprocessor_path from config file: {}",
                e
            ),
        }
    }

    // Browser-preview pipeline executable paths ([preview] section): same S1
    // trust boundary — TOML-only, never via the Web API.
    if let Some(command_path) = &file_config.preview.command_path {
        let db_guard = db.lock().await;
        match db_guard.set_preview_command_path(command_path) {
            Ok(()) => info!(
                "preview command_path set from config file: {}",
                command_path
            ),
            Err(e) => error!("Failed to set preview command_path from config file: {}", e),
        }
    }
    if let Some(preprocessor_path) = &file_config.preview.preprocessor_path {
        let db_guard = db.lock().await;
        match db_guard.set_preview_preprocessor_path(preprocessor_path) {
            Ok(()) => info!(
                "preview preprocessor_path set from config file: {}",
                preprocessor_path
            ),
            Err(e) => error!(
                "Failed to set preview preprocessor_path from config file: {}",
                e
            ),
        }
    }

    // 使用する B-CAS カードリーダーの指定。未選択 (空文字列) なら libaribb25 の
    // 既定動作のまま、見つかったリーダーへ順に接続を試す。B-CAS 以外のリーダーが
    // 挿さっている環境では、その1台につきリーダー起動が十数秒待たされ、しかも
    // 先に応答した方が採用されてしまうため、ダッシュボードから選べるようにしている。
    {
        let name = {
            let db_guard = db.lock().await;
            db_guard.get_card_reader_name().unwrap_or_default()
        };
        recisdb_proxy::apply_card_reader_selection(&name);
        // B25デコーダが使えるかを裏で判定しておく。最初の視聴要求のときに
        // 初めて調べると、応答しないカードリーダー相手では選局が
        // タイムアウトして視聴自体できなくなる。
        recisdb_proxy::tuner::shared::probe_b25_availability();
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
                warn!(
                    "Failed to persist web auth token override to database: {}",
                    e
                );
            }
            token.clone()
        } else {
            match db_guard.get_web_auth_token() {
                Ok(Some(token)) => token,
                Ok(None) => {
                    let generated = web::auth::generate_token();
                    if let Err(e) = db_guard.set_web_auth_token(&generated) {
                        warn!(
                            "Failed to persist generated web auth token to database: {}",
                            e
                        );
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
    let mirakurun_home_regions = resolved.mirakurun_home_regions.clone();
    let mirakurun_record_priority_threshold = resolved.mirakurun_record_priority_threshold;

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
                let (min_hold_secs, reject_cooldown_ms, no_data_timeout_secs) = db_lock
                    .get_tuner_livelock_config()
                    .unwrap_or((10, 2_000, 30));
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
                        min_hold_secs,
                        reject_cooldown_ms,
                        no_data_timeout_secs,
                        mmt_converter: Default::default(),
                    },
                    (
                        prefill_view_ms,
                        prefill_preview_ms,
                        prefill_record_ms,
                        jitter_safety_factor,
                    ),
                )
            }
            Err(e) => {
                warn!("Failed to load tuner config from database: {}", e);
                (TunerPoolConfig::default(), (1000, 2000, 6000, 1.5))
            }
        }
    };

    // The MMT/TLV converter is config-file-only (it names an executable), so
    // it is layered on after the DB-backed tuner settings rather than being
    // one of them.
    let tuner_config = {
        let mut cfg = tuner_config;
        cfg.mmt_converter = recisdb_proxy::tuner::mmt_pipe::MmtConverterConfig {
            command_path: file_config.mmttlv.command_path.clone().unwrap_or_default(),
            cas_proxy_server: file_config.mmttlv.cas_proxy_server.clone(),
            smart_card_reader_name: file_config.mmttlv.smart_card_reader_name.clone(),
            frontend_descrambled: file_config.mmttlv.frontend_descrambled,
            extra_args: file_config.mmttlv.extra_args.clone(),
        };
        if !cfg.mmt_converter.command_path.is_empty() {
            info!(
                "  MMT/TLV converter: {} (casProxy={:?}, reader={:?}, frontendDescrambled={})",
                cfg.mmt_converter.command_path,
                cfg.mmt_converter.cas_proxy_server,
                cfg.mmt_converter.smart_card_reader_name,
                cfg.mmt_converter.frontend_descrambled,
            );
        }
        cfg
    };

    // Build server config
    let mut config = ServerConfig {
        listen_addr,
        max_connections,
        // Replaced with the DB-derived value below after the optional default
        // tuner has been registered. Keep a conservative floor for an empty
        // database and avoid an arbitrary pool-wide cap for multi-BonDriver
        // installations.
        max_tuners: 16,
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

    // The pool-wide limit must not be smaller than the sum of the per-driver
    // limits.  The old hard-coded 16 caused Mirakurun/EPGStation requests to
    // fail with 503 after 16 readers existed, even when another registered
    // BonDriver had a free physical tuner.
    let configured_tuner_capacity = {
        let db_guard = db.lock().await;
        db_guard
            .get_all_bon_drivers()
            .map(|drivers| {
                drivers
                    .iter()
                    .map(|driver| driver.max_instances.max(0) as usize)
                    .sum::<usize>()
            })
            .unwrap_or(0)
    };
    config.max_tuners = config.max_tuners.max(configured_tuner_capacity);
    info!("  Tuner pool capacity: {}", config.max_tuners);

    // Create session registry for tracking active sessions
    let session_registry = Arc::new(web::SessionRegistry::new());

    // Start alert manager
    let alert_db = db.clone();
    let alert_registry = Arc::clone(&session_registry);
    tokio::spawn(async move {
        let manager = alert::AlertManager::new(alert_db, alert_registry);
        manager.run().await;
    });

    // Fan-out channel for `GET /mirakurun/api/events/stream`
    // (`web/mirakurun_events.rs`, `docs/EPGSTATION_COMPAT.md` §3/§6):
    // `EpgWriter` broadcasts every successfully-UPSERTed program row here,
    // `WebState` hands out `subscribe()`s to HTTP clients. Created once here
    // (not inside either `EpgWriter::new` or `WebState::new`) so both sides
    // share the exact same channel. Capacity 1024: `EpgWriter::flush` fires
    // at most every `FLUSH_INTERVAL` (10s) or `FLUSH_BATCH_SIZE` (500)
    // records, whichever comes first, so 1024 comfortably covers two full
    // batches of backlog for a subscriber that is briefly slow to drain —
    // beyond that, `RecvError::Lagged` is the correct outcome (handled by
    // logging and continuing, see `mirakurun_events.rs`), not a bigger
    // buffer.
    // The initial `Receiver` is dropped immediately (`_`, not `_rx`): holding
    // it would keep a subscriber alive for the whole process lifetime, so
    // every broadcast event would be retained in the ring buffer even when no
    // client has `/events/stream` open. With no subscribers, `send` returns
    // `Err` and the record is simply not buffered — which is what we want.
    let (epg_events_tx, _) = tokio::sync::broadcast::channel::<database::ProgramUpsert>(1024);

    // Start the EPG writer: batches EIT events collected by every live
    // tuner (`tuner/epg_collector.rs`) into the `programs` table. Must be
    // started before any tuner reader thread can run, since `EpgWriter::new`
    // installs the process-wide sender the collectors send into.
    let epg_db = db.clone();
    let epg_writer_events_tx = epg_events_tx.clone();
    tokio::spawn(async move {
        let writer = epg_writer::EpgWriter::new(epg_db, epg_writer_events_tx);
        writer.run().await;
    });

    // Start the NIT writer: fills the `channels` metadata that a scan would
    // normally supply (remote-control key / physical channel / network name)
    // for rows registered by hand, from the NIT seen on every live terrestrial
    // stream (`tuner/nit_collector.rs`). Same startup ordering requirement as
    // the EPG writer — it installs the process-wide sender its collectors
    // send into.
    let nit_db = db.clone();
    tokio::spawn(async move {
        let writer = nit_writer::NitWriter::new(nit_db);
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
        min_hold_secs: tuner_config.min_hold_secs,
        reject_cooldown_ms: tuner_config.reject_cooldown_ms,
        no_data_timeout_secs: tuner_config.no_data_timeout_secs,
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
    let web_log_level_handle = Arc::clone(&log_level_handle);
    let web_epg_events_tx = epg_events_tx.clone();

    // Distributed tuner fabric (`[node]`). The node-to-node transport is
    // always present on its dedicated listener. An unpaired node only waits
    // for pairing and does not expose any remote tuner to peers.
    let (node_transport_for_web, node_listen_display) = {
        let node_addr = resolved.node_listen_addr;
        let identity = {
            let db_lock = db.lock().await;
            match recisdb_proxy::node::NodeStore::new(&db_lock) {
                Ok(store) => match store.local_identity() {
                    Ok(mut identity) => {
                        // TOML display_name is a bootstrap default. Once the
                        // dashboard has saved a name, the DB is authoritative
                        // so a restart cannot silently undo a GUI edit.
                        if identity.display_name == "recisdb-proxy" {
                            if let Some(name) = resolved.node_display_name.clone() {
                                if name != identity.display_name {
                                    identity.display_name = name;
                                    if let Err(e) = store.update_local_identity(
                                        &identity,
                                        Some(&node_addr.to_string()),
                                    ) {
                                        warn!("Failed to persist node display name: {}", e);
                                    }
                                }
                            }
                        }
                        Some(identity)
                    }
                    Err(e) => {
                        error!(
                            "Node fabric unavailable: cannot load local node identity: {}",
                            e
                        );
                        None
                    }
                },
                Err(e) => {
                    error!("Node fabric unavailable: cannot open node store: {}", e);
                    None
                }
            }
        };

        match identity {
            Some(identity) => {
                let leases = Arc::new(recisdb_proxy::node::RemoteLeaseManager::new(
                    recisdb_proxy::node::LeasePolicy::default(),
                ));
                // Offering local tuners to peers goes through the same
                // `tuner::acquire` path as every local request, so a
                // remote recording contends under the same policy.
                let mux_server = Arc::new(recisdb_proxy::node::LocalMuxServer::new(
                    identity.clone(),
                    Arc::clone(server.tuner_pool()),
                    db.clone(),
                    Arc::clone(&leases),
                ));
                // Expired leases are normally noticed by their own pump
                // (it re-checks the lease every second and stops when it
                // is gone). This janitor is the backstop for a lease whose
                // pump is no longer running — otherwise the entry would
                // sit in the map forever, and `GET /api/nodes` would keep
                // reporting a lease nobody holds.
                let reaper = Arc::clone(&leases);
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                    loop {
                        ticker.tick().await;
                        for lease in reaper.reap_expired().await {
                            warn!(
                                "[node] lease {} expired without a renew; released",
                                lease.id.as_str()
                            );
                        }
                    }
                });
                let state = Arc::new(
                    recisdb_proxy::node::NodeTransportState::new(identity.clone(), leases)
                        .with_database(db.clone())
                        .with_mux_server(mux_server),
                );
                match state.reload_peers().await {
                    Ok(count) => info!(
                        "Node fabric listening as {} (\"{}\"), {} paired peer(s)",
                        identity.node_id, identity.display_name, count
                    ),
                    Err(e) => warn!("Failed to load paired node credentials: {}", e),
                }
                // Advertise our own reception routes and pull the peers'
                // back, so candidate discovery has a stored picture
                // instead of a round trip per request.
                recisdb_proxy::node::RouteSync::new(
                    Arc::clone(&state),
                    db.clone(),
                    Arc::clone(server.tuner_pool()),
                )
                .spawn();

                let serve_state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = recisdb_proxy::node::serve_h2c(node_addr, serve_state).await {
                        error!("Node transport listener stopped: {}", e);
                    }
                });
                (Some(state), Some(node_addr.to_string()))
            }
            None => (None, None),
        }
    };
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
            web_log_level_handle,
            mirakurun_enabled,
            mirakurun_home_regions,
            mirakurun_record_priority_threshold,
            Some(listen_addr),
            config_path_for_web,
            web_epg_events_tx,
            node_transport_for_web,
            node_listen_display,
        )
        .await
        {
            Ok(_) => info!("Web dashboard server stopped"),
            Err(e) => error!("Web dashboard error: {}", e),
        }
    });

    info!("Web dashboard listening on http://{}", web_listen_addr);

    // Load scan scheduler configuration from database
    let (
        db_check_interval,
        db_max_concurrent,
        db_timeout,
        db_signal_lock_wait_ms,
        db_ts_read_timeout_ms,
    ) = {
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
                (
                    args.scan_interval,
                    args.max_concurrent_scans,
                    900,
                    500,
                    300000,
                )
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

        info!(
            "Starting channel scan scheduler (interval: {}s, max concurrent: {})",
            db_check_interval, db_max_concurrent
        );
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
