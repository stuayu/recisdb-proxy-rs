//! TCP connection management for the BonDriver client.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use recisdb_protocol::{
    decode_header, decode_server_message, encode_client_message, ClientMessage,
    ServerMessage, HEADER_SIZE, PROTOCOL_VERSION,
};

use crate::client::buffer::TsRingBuffer;
use crate::file_log;

#[cfg(feature = "tls")]
use std::fs::File;
#[cfg(feature = "tls")]
use std::io::BufReader;
#[cfg(feature = "tls")]
use std::path::Path;
#[cfg(feature = "tls")]
use rustls::pki_types::ServerName;
#[cfg(feature = "tls")]
use tokio_rustls::TlsConnector;

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    TunerOpen,
    Streaming,
    Error,
}

/// Connection configuration.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub server_addr: String,
    pub tuner_path: String,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Default client priority sent with channel set requests.
    pub client_priority: i32,
    /// Default exclusive lock flag sent with channel set requests.
    pub client_exclusive: bool,
    /// Enable TLS connection.
    #[cfg(feature = "tls")]
    pub tls_enabled: bool,
    /// Path to CA certificate for TLS verification.
    #[cfg(feature = "tls")]
    pub tls_ca_cert: Option<String>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            server_addr: "127.0.0.1:12345".to_string(),
            tuner_path: String::new(),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(5),
            client_priority: 0,
            client_exclusive: false,
            #[cfg(feature = "tls")]
            tls_enabled: false,
            #[cfg(feature = "tls")]
            tls_ca_cert: None,
        }
    }
}

/// Manages the TCP connection to the proxy server.
pub struct Connection {
    /// Configuration.
    config: ConnectionConfig,
    /// Current state.
    state: Mutex<ConnectionState>,
    /// Ring buffer for TS data.
    buffer: Arc<TsRingBuffer>,
    /// Channel for sending requests.
    request_tx: Mutex<Option<mpsc::Sender<ClientMessage>>>,
    /// Channel for receiving responses.
    response_rx: Mutex<Option<mpsc::Receiver<ServerMessage>>>,
    /// Tokio runtime handle.
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    /// BonDriver version reported by server.
    bondriver_version: Mutex<u8>,
    /// Last signal level.
    signal_level: Mutex<f32>,
}

impl Connection {
    /// Create a new connection.
    pub fn new(config: ConnectionConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Mutex::new(ConnectionState::Disconnected),
            buffer: Arc::new(TsRingBuffer::new()),
            request_tx: Mutex::new(None),
            response_rx: Mutex::new(None),
            runtime: Mutex::new(None),
            bondriver_version: Mutex::new(0),
            signal_level: Mutex::new(0.0),
        })
    }

    /// Get the current state.
    pub fn state(&self) -> ConnectionState {
        *self.state.lock()
    }

    /// Get the BonDriver version.
    #[allow(dead_code)]
    pub fn bondriver_version(&self) -> u8 {
        *self.bondriver_version.lock()
    }

    /// Get the signal level.
    #[allow(dead_code)]
    pub fn signal_level(&self) -> f32 {
        *self.signal_level.lock()
    }

    /// Get default client priority from configuration.
    pub fn default_priority(&self) -> i32 {
        self.config.client_priority
    }

    /// Get default exclusive lock flag from configuration.
    pub fn default_exclusive(&self) -> bool {
        self.config.client_exclusive
    }

    /// Get a reference to the ring buffer.
    pub fn buffer(&self) -> &Arc<TsRingBuffer> {
        &self.buffer
    }

    /// Connect to the server.
    pub fn connect(self: &Arc<Self>) -> bool {
        file_log!(info, "Connection::connect() called");

        let mut state = self.state.lock();
        file_log!(debug, "connect: Current state = {:?}", *state);
        if *state != ConnectionState::Disconnected {
            file_log!(warn, "connect: Already connected or connecting, state = {:?}", *state);
            return false;
        }
        *state = ConnectionState::Connecting;
        drop(state);

        // Create runtime
        file_log!(info, "connect: Creating tokio runtime...");
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => {
                file_log!(info, "connect: Tokio runtime created successfully");
                rt
            }
            Err(e) => {
                file_log!(error, "connect: Failed to create runtime: {}", e);
                error!("Failed to create runtime: {}", e);
                *self.state.lock() = ConnectionState::Error;
                return false;
            }
        };

        file_log!(debug, "connect: Creating channels...");
        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<ServerMessage>(32);

        *self.request_tx.lock() = Some(req_tx);
        *self.response_rx.lock() = Some(resp_rx);

        let conn = Arc::clone(self);
        let config = self.config.clone();
        let buffer = Arc::clone(&self.buffer);

        file_log!(info, "connect: Spawning connection task to {}", config.server_addr);
        runtime.spawn(async move {
            file_log!(info, "connect: Connection task started");
            if let Err(e) = connection_task(conn, config, req_rx, resp_tx, buffer).await {
                file_log!(error, "connect: Connection task error: {}", e);
                error!("Connection task error: {}", e);
            }
            file_log!(info, "connect: Connection task ended");
        });

        *self.runtime.lock() = Some(runtime);

        // Wait for connection task to establish TCP connection
        // Use a reasonable fixed wait time based on connect_timeout
        let wait_time = self.config.connect_timeout.min(Duration::from_secs(5));
        let sleep_time = wait_time.min(Duration::from_millis(500));
        file_log!(debug, "connect: Waiting {:?} for connection...", sleep_time);
        std::thread::sleep(sleep_time);

        // Perform handshake with timeout
        file_log!(info, "connect: Sending hello...");
        if !self.send_hello() {
            file_log!(error, "connect: Handshake failed");
            error!("Handshake failed");
            *self.state.lock() = ConnectionState::Error;
            return false;
        }

        file_log!(info, "connect: Connected successfully");
        *self.state.lock() = ConnectionState::Connected;
        true
    }

    /// Disconnect from the server.
    pub fn disconnect(&self) {
        // Drop the request channel to signal shutdown
        *self.request_tx.lock() = None;
        *self.response_rx.lock() = None;

        // Shutdown runtime
        if let Some(rt) = self.runtime.lock().take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }

        self.buffer.clear();
        *self.state.lock() = ConnectionState::Disconnected;
    }

    /// Send a message and wait for response with timeout.
    fn send_request_with_timeout(&self, msg: ClientMessage, timeout: Duration) -> Option<ServerMessage> {
        let tx = self.request_tx.lock();
        let tx = tx.as_ref()?;

        // Send request
        debug!("[Connection] Sending message: {:?}", std::mem::discriminant(&msg));
        if tx.blocking_send(msg).is_err() {
            error!("[Connection] Failed to send request to server");
            return None;
        }
        debug!("[Connection] Message sent successfully, waiting for response (timeout: {:?})", timeout);

        // Wait for response with timeout using polling
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(5);
        let mut poll_count = 0;

        loop {
            {
                let mut rx = self.response_rx.lock();
                if let Some(rx) = rx.as_mut() {
                    // Try non-blocking receive
                    match rx.try_recv() {
                        Ok(resp) => {
                            debug!("[Connection] Received response after {} polls", poll_count);
                            return Some(resp);
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {
                            // No message yet, continue polling
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            error!("[Connection] Response channel closed");
                            return None;
                        }
                    }
                } else {
                    error!("[Connection] Response channel not initialized");
                    return None;
                }
            }

            // Check timeout
            if start.elapsed() >= timeout {
                warn!("[Connection] Request timed out after {:?} ({} polls)", timeout, poll_count);
                return None;
            }

            poll_count += 1;
            std::thread::sleep(poll_interval);
        }
    }

    /// Send a message and wait for response (using configured read timeout).
    fn send_request(&self, msg: ClientMessage) -> Option<ServerMessage> {
        self.send_request_with_timeout(msg, self.config.read_timeout)
    }

    /// Send hello message with timeout (for connection setup).
    #[allow(dead_code)]
    fn send_hello_with_timeout(&self, timeout: Duration) -> bool {
        let resp = self.send_request_with_timeout(
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            timeout,
        );

        match resp {
            Some(ServerMessage::HelloAck { version, success }) => {
                if success {
                    info!("Connected to server, protocol version {}", version);
                    true
                } else {
                    error!("Server rejected hello, version mismatch");
                    false
                }
            }
            _ => {
                // No response yet or invalid response
                false
            }
        }
    }

    /// Send hello message.
    fn send_hello(&self) -> bool {
        let resp = self.send_request(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        });

        match resp {
            Some(ServerMessage::HelloAck { version, success }) => {
                if success {
                    info!("Connected to server, protocol version {}", version);
                    true
                } else {
                    error!("Server rejected hello, version mismatch");
                    false
                }
            }
            _ => {
                error!("Invalid hello response");
                false
            }
        }
    }

    /// Open a tuner.
    pub fn open_tuner(&self) -> bool {
        let state = self.state();
        if state != ConnectionState::Connected && state != ConnectionState::TunerOpen {
            return false;
        }

        let resp = self.send_request(ClientMessage::OpenTuner {
            tuner_path: self.config.tuner_path.clone(),
        });

        match resp {
            Some(ServerMessage::OpenTunerAck {
                success,
                bondriver_version,
                ..
            }) => {
                if success {
                    *self.bondriver_version.lock() = bondriver_version;
                    *self.state.lock() = ConnectionState::TunerOpen;
                    info!("Tuner opened, BonDriver version {}", bondriver_version);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Close the tuner.
    pub fn close_tuner(&self) {
        if self.state() == ConnectionState::Streaming {
            self.stop_stream();
        }

        let _ = self.send_request(ClientMessage::CloseTuner);
        *self.state.lock() = ConnectionState::Connected;
    }

    /// Set channel (IBonDriver v1).
    pub fn set_channel(&self, channel: u8, _force: bool) -> bool {
        let resp = self.send_request(ClientMessage::SetChannel {
            channel,
            priority: self.config.client_priority,
            exclusive: self.config.client_exclusive,
        });

        match resp {
            Some(ServerMessage::SetChannelAck { success, .. }) => success,
            _ => false,
        }
    }

    /// Set channel by space (IBonDriver v2).
    pub fn set_channel_space(&self, space: u32, channel: u32, priority: i32, exclusive: bool) -> bool {
        let resp = self.send_request(ClientMessage::SetChannelSpace { space, channel, priority, exclusive });

        match resp {
            Some(ServerMessage::SetChannelSpaceAck { success, .. }) => success,
            _ => false,
        }
    }

    /// Get signal level.
    pub fn get_signal_level(&self) -> f32 {
        let resp = self.send_request(ClientMessage::GetSignalLevel);

        match resp {
            Some(ServerMessage::GetSignalLevelAck { signal_level }) => {
                *self.signal_level.lock() = signal_level;
                signal_level
            }
            _ => *self.signal_level.lock(),
        }
    }

    /// Start streaming.
    pub fn start_stream(&self) -> bool {
        if self.state() != ConnectionState::TunerOpen {
            return false;
        }

        let resp = self.send_request(ClientMessage::StartStream);

        match resp {
            Some(ServerMessage::StartStreamAck { success, .. }) => {
                if success {
                    *self.state.lock() = ConnectionState::Streaming;
                }
                success
            }
            _ => false,
        }
    }

    /// Stop streaming.
    pub fn stop_stream(&self) {
        if self.state() != ConnectionState::Streaming {
            return;
        }

        let _ = self.send_request(ClientMessage::StopStream);
        *self.state.lock() = ConnectionState::TunerOpen;
    }

    /// Purge stream buffer.
    pub fn purge_stream(&self) {
        self.buffer.clear();
        let _ = self.send_request(ClientMessage::PurgeStream);
    }

    /// Enumerate tuning space.
    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        let resp = self.send_request(ClientMessage::EnumTuningSpace { space });

        match resp {
            Some(ServerMessage::EnumTuningSpaceAck { name }) => name,
            _ => None,
        }
    }

    /// Enumerate channel name.
    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        let resp = self.send_request(ClientMessage::EnumChannelName { space, channel });

        match resp {
            Some(ServerMessage::EnumChannelNameAck { name }) => name,
            _ => None,
        }
    }

    /// Set LNB power.
    pub fn set_lnb_power(&self, enable: bool) -> bool {
        let resp = self.send_request(ClientMessage::SetLnbPower { enable });

        match resp {
            Some(ServerMessage::SetLnbPowerAck { success, .. }) => success,
            _ => false,
        }
    }
}

/// Background task for handling the connection.
async fn connection_task(
    conn: Arc<Connection>,
    config: ConnectionConfig,
    req_rx: mpsc::Receiver<ClientMessage>,
    resp_tx: mpsc::Sender<ServerMessage>,
    buffer: Arc<TsRingBuffer>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    file_log!(info, "connection_task: Starting, connecting to {}...", config.server_addr);
    info!("Connecting to {}...", config.server_addr);

    file_log!(debug, "connection_task: Attempting TCP connect with timeout {:?}", config.connect_timeout);
    let stream = match tokio::time::timeout(
        config.connect_timeout,
        TcpStream::connect(&config.server_addr),
    )
    .await {
        Ok(Ok(s)) => {
            file_log!(info, "connection_task: TCP connection established");
            s
        }
        Ok(Err(e)) => {
            file_log!(error, "connection_task: TCP connect failed: {}", e);
            return Err(e.into());
        }
        Err(e) => {
            file_log!(error, "connection_task: TCP connect timeout: {}", e);
            return Err(e.into());
        }
    };

    stream.set_nodelay(true)?;
    file_log!(info, "connection_task: Connected to {}", config.server_addr);
    info!("Connected to {}", config.server_addr);

    // Handle TLS if enabled
    #[cfg(feature = "tls")]
    {
        if config.tls_enabled {
            info!("Establishing TLS connection...");
            let tls_config = build_tls_config(config.tls_ca_cert.as_deref())?;
            let connector = TlsConnector::from(Arc::new(tls_config));
            let server_name = extract_server_name(&config.server_addr);

            let tls_stream = connector.connect(server_name, stream).await?;
            info!("TLS connection established");

            let (reader, writer) = tokio::io::split(tls_stream);
            return connection_loop(conn, req_rx, resp_tx, buffer, reader, writer).await;
        }
    }

    // Plain TCP connection
    let (reader, writer) = stream.into_split();
    connection_loop(conn, req_rx, resp_tx, buffer, reader, writer).await
}

/// Main connection loop handling reads and writes.
async fn connection_loop<R, W>(
    conn: Arc<Connection>,
    mut req_rx: mpsc::Receiver<ClientMessage>,
    resp_tx: mpsc::Sender<ServerMessage>,
    buffer: Arc<TsRingBuffer>,
    mut reader: R,
    mut writer: W,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut read_buf = BytesMut::with_capacity(65536);

    loop {
        tokio::select! {
            // Handle outgoing requests
            Some(msg) = req_rx.recv() => {
                trace!("Sending request: {:?}", msg);
                let encoded = encode_client_message(&msg)?;
                writer.write_all(&encoded).await?;
            }

            // Handle incoming data
            result = reader.read_buf(&mut read_buf) => {
                let n = result?;
                if n == 0 {
                    info!("Connection closed by server");
                    *conn.state.lock() = ConnectionState::Disconnected;
                    break;
                }

                // Process complete frames
                while read_buf.len() >= HEADER_SIZE {
                    match decode_header(&read_buf)? {
                        Some(header) => {
                            let total_len = HEADER_SIZE + header.payload_len as usize;
                            if read_buf.len() >= total_len {
                                let _ = read_buf.split_to(HEADER_SIZE);
                                let payload = read_buf.split_to(header.payload_len as usize);
                                let payload = Bytes::from(payload.to_vec());

                                let msg = decode_server_message(header.message_type, payload)?;

                                // Handle TS data specially
                                if let ServerMessage::TsData { data } = &msg {
                                    static TS_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                    static TS_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

                                    let count = TS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    TS_BYTES.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);

                                    let written = buffer.write(data);

                                    // Log every 100 messages
                                    if count % 100 == 0 {
                                        let total_bytes = TS_BYTES.load(std::sync::atomic::Ordering::Relaxed);
                                        crate::file_log!(info, "TsData #{}: {} bytes, written={}, buffer={}, total={}",
                                               count, data.len(), written, buffer.available(), total_bytes);
                                    }

                                    if written < data.len() {
                                        crate::file_log!(warn, "Buffer full, dropped {} bytes", data.len() - written);
                                    }
                                } else {
                                    // Send response to waiting request
                                    if resp_tx.send(msg).await.is_err() {
                                        debug!("Response channel closed");
                                    }
                                }
                            } else {
                                break; // Need more data
                            }
                        }
                        None => break, // Need more data
                    }
                }
            }
        }
    }

    Ok(())
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.disconnect();
    }
}

// =============================================================================
// TLS Support
// =============================================================================

/// Build TLS client configuration.
#[cfg(feature = "tls")]
fn build_tls_config(ca_cert_path: Option<&str>) -> Result<rustls::ClientConfig, Box<dyn std::error::Error + Send + Sync>> {
    use rustls::RootCertStore;
    use rustls_pemfile::certs;

    let mut root_store = RootCertStore::empty();

    if let Some(ca_path) = ca_cert_path {
        // Load custom CA certificate
        let ca_file = File::open(Path::new(ca_path))?;
        let mut ca_reader = BufReader::new(ca_file);
        let certs_result: Vec<_> = certs(&mut ca_reader).collect();

        for cert in certs_result {
            let cert = cert?;
            root_store.add(cert)?;
        }
        info!("Loaded CA certificate from {}", ca_path);
    } else {
        // Use system root certificates
        match rustls_native_certs::load_native_certs() {
            Ok(certs) => {
                for cert in certs {
                    let _ = root_store.add(cert);
                }
                debug!("Loaded system root certificates");
            }
            Err(e) => {
                warn!("Failed to load system root certificates: {}", e);
            }
        }
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(config)
}

/// Extract server name from address for TLS SNI.
#[cfg(feature = "tls")]
fn extract_server_name(addr: &str) -> ServerName<'static> {
    // Try to parse as host:port
    let host = if let Some(colon_pos) = addr.rfind(':') {
        // Check if it's an IPv6 address
        if addr.starts_with('[') {
            if let Some(bracket_pos) = addr.find(']') {
                // [ipv6]:port format
                &addr[1..bracket_pos]
            } else {
                &addr[..colon_pos]
            }
        } else {
            &addr[..colon_pos]
        }
    } else {
        addr
    };

    // Try to parse as DNS name first
    match ServerName::try_from(host.to_string()) {
        Ok(name) => name,
        Err(_) => {
            // Fall back to localhost
            ServerName::try_from("localhost".to_string()).unwrap()
        }
    }
}
