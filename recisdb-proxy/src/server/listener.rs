//! TCP listener for accepting client connections.

use std::net::SocketAddr;
use std::sync::Arc;

use log::{error, info};
use tokio::net::{TcpListener, TcpStream};

use crate::database::Database;
use crate::server::session::Session;
use crate::tuner::TunerPool;
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
    database: DatabaseHandle,
    session_registry: Arc<SessionRegistry>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: ServerConfig, session_registry: Arc<SessionRegistry>) -> Self {
        let database = config.database.clone();
        Self {
            config,
            tuner_pool: Arc::new(TunerPool::default()),
            database,
            session_registry,
        }
    }

    /// Run the server, accepting connections until shutdown.
    pub async fn run(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        info!("Server listening on {}", self.config.listen_addr);

        let mut connection_count = 0u64;

        loop {
            match listener.accept().await {
                Ok((socket, addr)) => {
                    connection_count += 1;
                    let session_id = connection_count;

                    info!("[Session {}] New connection from {}", session_id, addr);

                    let pool = Arc::clone(&self.tuner_pool);
                    let database = Arc::clone(&self.database);
                    let default_tuner = self.config.default_tuner.clone();
                    let session_registry = Arc::clone(&self.session_registry);

                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(socket, addr, session_id, pool, database, default_tuner, session_registry).await {
                            error!("[Session {}] Connection error: {}", session_id, e);
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
    database: DatabaseHandle,
    default_tuner: Option<String>,
    session_registry: Arc<SessionRegistry>,
) -> std::io::Result<()> {
    // Disable Nagle's algorithm for lower latency
    socket.set_nodelay(true)?;

    // Register the session
    let shutdown_rx = session_registry.register(session_id, addr).await;

    let mut session = Session::new(
        session_id,
        addr,
        socket,
        tuner_pool,
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
