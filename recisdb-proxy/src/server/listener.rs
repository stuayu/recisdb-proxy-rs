//! TCP listener for accepting client connections.

use std::net::SocketAddr;
use std::sync::Arc;

use log::{error, info, warn};
use tokio::io::{AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use bytes::Bytes;

use crate::database::Database;
use crate::server::session::Session;
use crate::server::ts_queue::{TsQueueShared, TsWriteQueue};
use crate::tuner::{EncoderPool, TunerPool, TunerPoolConfig};
use crate::web::SessionRegistry;

/// Database handle type.
pub type DatabaseHandle = Arc<tokio::sync::Mutex<Database>>;

/// Server configuration.
#[derive(Clone)]
pub struct ServerConfig {
    /// Address to listen on.
    pub listen_addr: SocketAddr,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Path to the default tuner device.
    pub default_tuner: Option<String>,
    /// Database handle.
    pub database: DatabaseHandle,
    /// Tuner optimization configuration.
    pub tuner_config: TunerPoolConfig,
    /// TLS configuration (optional).
    #[cfg(feature = "tls")]
    pub tls_config: Option<TlsConfig>,
}

/// TLS configuration.
#[cfg(feature = "tls")]
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub ca_cert_path: String,
    pub server_cert_path: String,
    pub server_key_path: String,
    pub require_client_cert: bool,
}

/// The main server that listens for connections and spawns sessions.
pub struct Server {
    config: ServerConfig,
    tuner_pool: Arc<TunerPool>,
    encoder_pool: Arc<EncoderPool>,
    database: DatabaseHandle,
    session_registry: Arc<SessionRegistry>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: ServerConfig, session_registry: Arc<SessionRegistry>) -> Self {
        let database = config.database.clone();
        let tuner_config = config.tuner_config.clone();
        Self {
            config,
            tuner_pool: Arc::new(TunerPool::new_with_config(16, tuner_config)),
            // Shared tsreplace encoder pool (STREAMING_DESIGN.md §5 P4).
            // The concurrency cap is re-synced from tsreplace_config each time
            // a session starts an encoder, so the initial value here only
            // covers the window before the first DB read.
            encoder_pool: Arc::new(EncoderPool::default()),
            database,
            session_registry,
        }
    }

    /// Run the server, accepting connections until shutdown.
    pub async fn run(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        info!("Server listening on {}", self.config.listen_addr);

        loop {
            match listener.accept().await {
                Ok((socket, addr)) => {
                    // Session ids come from the registry so that BNDP and the
                    // HTTP stream paths (`web/http_session.rs`) never hand out
                    // the same id — they share one map and one disconnect API.
                    let session_id = self.session_registry.allocate_id();

                    info!("[Session {}] New connection from {}", session_id, addr);

                    let pool = Arc::clone(&self.tuner_pool);
                    let encoder_pool = Arc::clone(&self.encoder_pool);
                    let database = Arc::clone(&self.database);
                    let default_tuner = self.config.default_tuner.clone();
                    let session_registry = Arc::clone(&self.session_registry);

                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(socket, addr, session_id, pool, encoder_pool, database, default_tuner, session_registry).await {
                            // Client-initiated socket teardown (TVTest exit,
                            // EDCB reconnect cycle, …) surfaces here as
                            // BrokenPipe/ConnectionReset/ConnectionAborted —
                            // an expected disconnect, not a server fault.
                            match e.kind() {
                                std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::UnexpectedEof => {
                                    info!("[Session {}] Disconnected by client: {}", session_id, e);
                                }
                                _ => {
                                    error!("[Session {}] Connection error: {}", session_id, e);
                                }
                            }
                        }
                        info!("[Session {}] Connection closed", session_id);
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    /// Get a reference to the tuner pool.
    pub fn tuner_pool(&self) -> &Arc<TunerPool> {
        &self.tuner_pool
    }

    /// Get a reference to the shared encoder pool.
    ///
    /// Also handed to the web server (`main.rs`) so HTTP `?profile=preview`
    /// requests (STREAMING_DESIGN.md §6.3 P5) join the same encoder chains as
    /// BNDP sessions instead of spawning their own.
    pub fn encoder_pool(&self) -> &Arc<EncoderPool> {
        &self.encoder_pool
    }

    /// Get a reference to the database.
    pub fn database(&self) -> &DatabaseHandle {
        &self.database
    }
}

/// Handle a single client connection.
async fn handle_connection(
    socket: TcpStream,
    addr: SocketAddr,
    session_id: u64,
    tuner_pool: Arc<TunerPool>,
    encoder_pool: Arc<EncoderPool>,
    database: DatabaseHandle,
    default_tuner: Option<String>,
    session_registry: Arc<SessionRegistry>,
) -> std::io::Result<()> {
    // Disable Nagle's algorithm for lower latency
    socket.set_nodelay(true)?;

    // Split the socket into independent read/write halves.
    // The write half moves to a dedicated writer task so that socket writes
    // (which may block on TCP backpressure) never stall the main select loop.
    let (reader, writer) = socket.into_split();

    // Per-session write channels.
    // TS data  :  bounded by a byte budget (see `ts_queue`), sized in seconds
    //             of stream once the session knows its class and bitrate.
    // Control  :  bounded but generous, uses send().await (low volume).
    let (ts_write_queue, ts_write_rx, ts_queue_shared) =
        TsWriteQueue::new(crate::server::ts_queue::DEFAULT_BUDGET_BYTES);
    let (ctrl_write_tx, ctrl_write_rx) = mpsc::channel::<Bytes>(
        Session::CTRL_WRITE_BUFFER_CAPACITY,
    );

    // Spawn the writer task – it owns the write-half of the socket.
    let writer_handle = tokio::spawn(
        session_writer(session_id, writer, ts_write_rx, ctrl_write_rx, ts_queue_shared),
    );

    // Register the session
    let shutdown_rx = session_registry
        .register(session_id, addr, crate::web::SessionProtocol::Bndp)
        .await;

    let mut session = Session::new(
        session_id,
        addr,
        reader,
        ts_write_queue,
        ctrl_write_tx,
        writer_handle,
        tuner_pool,
        encoder_pool,
        database,
        default_tuner,
        Arc::clone(&session_registry),
        shutdown_rx,
    );
    let result = session.run().await;

    // Unregister the session when done
    session_registry.unregister(session_id).await;

    result
}

/// Dedicated per-session writer task.
///
/// Drains two channels (control messages with priority, TS data) and writes
/// the pre-encoded frames to the socket.  By running in its own task the
/// socket write calls — which may block for an extended period during
/// network congestion — never stall the session's broadcast receiver or
/// command handler.
///
/// The function exits when both channels are closed (session drop) or when a
/// socket write error occurs.
async fn session_writer(
    session_id: u64,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut ts_rx: mpsc::Receiver<Bytes>,
    mut ctrl_rx: mpsc::Receiver<Bytes>,
    ts_queue: Arc<TsQueueShared>,
) {
    // Every TS frame that leaves this task must give its bytes back to the
    // queue budget, including on the error paths — otherwise the budget leaks
    // and the session throttles itself to a standstill.
    macro_rules! write_ts {
        ($data:expr) => {{
            let data = $data;
            let len = data.len();
            let result = writer.write_all(&data).await;
            ts_queue.release(len);
            result
        }};
    }

    loop {
        tokio::select! {
            biased;

            // --- Priority: control messages first ---
            msg = ctrl_rx.recv() => {
                match msg {
                    Some(data) => {
                        if let Err(e) = writer.write_all(&data).await {
                            warn!("[Session {} writer] Control write error: {}", session_id, e);
                            return;
                        }
                        if let Err(e) = writer.flush().await {
                            warn!("[Session {} writer] Flush error after ctrl: {}", session_id, e);
                            return;
                        }
                    }
                    None => {
                        // ctrl channel closed – session is shutting down.
                        // Drain remaining TS frames before exiting.
                        while let Ok(data) = ts_rx.try_recv() {
                            if write_ts!(data).is_err() { return; }
                        }
                        let _ = writer.flush().await;
                        return;
                    }
                }
            }

            // --- Bulk: TS data (batch-drain for throughput) ---
            msg = ts_rx.recv() => {
                match msg {
                    Some(data) => {
                        if let Err(e) = write_ts!(data) {
                            warn!("[Session {} writer] TS write error: {}", session_id, e);
                            return;
                        }
                        // Drain all immediately-available TS frames in one
                        // batch before flushing – this reduces the number of
                        // syscalls under high throughput.
                        loop {
                            // Always check ctrl first inside the drain loop
                            // so that interleaved control messages are not
                            // delayed until the TS batch ends.
                            match ctrl_rx.try_recv() {
                                Ok(ctrl_data) => {
                                    if let Err(e) = writer.write_all(&ctrl_data).await {
                                        warn!("[Session {} writer] Control write error: {}", session_id, e);
                                        return;
                                    }
                                }
                                Err(_) => {}
                            }
                            match ts_rx.try_recv() {
                                Ok(ts_data) => {
                                    if let Err(e) = write_ts!(ts_data) {
                                        warn!("[Session {} writer] TS write error: {}", session_id, e);
                                        return;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        if let Err(e) = writer.flush().await {
                            warn!("[Session {} writer] Flush error after TS: {}", session_id, e);
                            return;
                        }
                    }
                    None => {
                        // TS channel closed.
                        let _ = writer.flush().await;
                        return;
                    }
                }
            }
        }
    }
}
