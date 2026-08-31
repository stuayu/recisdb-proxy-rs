//! TCP connection management for the BonDriver client.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};

use recisdb_protocol::{
    decode_header, decode_server_message, encode_client_message, ClientMessage, MessageType,
    ServerMessage, StreamClass, HEADER_SIZE, PROTOCOL_VERSION,
};

use crate::client::buffer::TsRingBuffer;
use crate::file_log;

#[cfg(feature = "tls")]
use rustls::pki_types::ServerName;
#[cfg(feature = "tls")]
use std::fs::File;
#[cfg(feature = "tls")]
use std::io::BufReader;
#[cfg(feature = "tls")]
use std::path::Path;
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
    /// Transient: the connection dropped unexpectedly and the supervisor is
    /// re-establishing it in the background.  Only used when no tuner was open
    /// at the time of the drop; while a tuner/stream is active the public state
    /// is kept at `TunerOpen`/`Streaming` so the synchronous FFI surface
    /// (IsTunerOpening / GetActiveDeviceNum) continues to report a plausible
    /// "still open" state during the outage.
    Reconnecting,
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
    /// Single-service filter mode.
    /// When true, the server sends only the selected service's TS packets
    /// instead of the entire transport stream.
    pub single_service: bool,
    /// Stream reliability class sent in `Hello` (STREAMING_DESIGN.md §2/§10).
    /// Sent as part of the protocol v2 `Hello` payload; a v2 server also
    /// auto-promotes VIEW/PREVIEW sessions to RECORD when the effective
    /// channel priority is high enough, so this is a hint, not the only path.
    pub stream_class: StreamClass,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            server_addr: "127.0.0.1:40070".to_string(),
            tuner_path: String::new(),
            // Same values the sample INI documents — this impl is the single
            // source of truth and the INI/env loaders fall back to it, so the
            // documented default and the compiled-in one cannot drift apart.
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(30),
            client_priority: 0,
            client_exclusive: false,
            #[cfg(feature = "tls")]
            tls_enabled: false,
            #[cfg(feature = "tls")]
            tls_ca_cert: None,
            single_service: false,
            stream_class: StreamClass::View,
        }
    }
}

/// Can `resp` be the answer to `req`?
///
/// The protocol has no correlation id, so this type pairing is the only way to
/// notice that the response channel has slipped out of step (see
/// [`Connection::send_request_with_timeout`]). `Error` is accepted for every
/// request because that is how the server refuses any of them.
fn is_reply_to(req: &ClientMessage, resp: &ServerMessage) -> bool {
    use ClientMessage as C;
    use ServerMessage as S;

    if matches!(resp, S::Error { .. }) {
        return true;
    }

    matches!(
        (req, resp),
        (C::Hello { .. }, S::HelloAck { .. })
            | (C::Ping, S::Pong)
            | (
                C::OpenTuner { .. } | C::OpenTunerWithGroup { .. },
                S::OpenTunerAck { .. }
            )
            | (C::CloseTuner, S::CloseTunerAck { .. })
            | (C::SetChannel { .. }, S::SetChannelAck { .. })
            | (
                C::SetChannelSpace { .. } | C::SetChannelSpaceInGroup { .. },
                S::SetChannelSpaceAck { .. }
            )
            | (C::GetSignalLevel, S::GetSignalLevelAck { .. })
            | (C::EnumTuningSpace { .. }, S::EnumTuningSpaceAck { .. })
            | (C::EnumChannelName { .. }, S::EnumChannelNameAck { .. })
            | (C::StartStream, S::StartStreamAck { .. })
            | (C::StopStream, S::StopStreamAck { .. })
            | (C::PurgeStream, S::PurgeStreamAck { .. })
            | (C::SetLnbPower { .. }, S::SetLnbPowerAck { .. })
            | (
                C::SelectLogicalChannel { .. },
                S::SelectLogicalChannelAck { .. }
            )
            | (C::GetChannelList { .. }, S::GetChannelListAck { .. })
            | (C::SetServiceFilter { .. }, S::SetServiceFilterAck { .. })
    )
}

fn client_message_name(msg: &ClientMessage) -> &'static str {
    match msg {
        ClientMessage::Hello { .. } => "Hello",
        ClientMessage::Ping => "Ping",
        ClientMessage::OpenTuner { .. } => "OpenTuner",
        ClientMessage::OpenTunerWithGroup { .. } => "OpenTunerWithGroup",
        ClientMessage::CloseTuner => "CloseTuner",
        ClientMessage::SetChannel { .. } => "SetChannel",
        ClientMessage::SetChannelSpace { .. } => "SetChannelSpace",
        ClientMessage::SetChannelSpaceInGroup { .. } => "SetChannelSpaceInGroup",
        ClientMessage::GetSignalLevel => "GetSignalLevel",
        ClientMessage::EnumTuningSpace { .. } => "EnumTuningSpace",
        ClientMessage::EnumChannelName { .. } => "EnumChannelName",
        ClientMessage::StartStream => "StartStream",
        ClientMessage::StopStream => "StopStream",
        ClientMessage::PurgeStream => "PurgeStream",
        ClientMessage::SetLnbPower { .. } => "SetLnbPower",
        ClientMessage::SelectLogicalChannel { .. } => "SelectLogicalChannel",
        ClientMessage::GetChannelList { .. } => "GetChannelList",
        ClientMessage::SetServiceFilter { .. } => "SetServiceFilter",
    }
}

/// Last channel selection, remembered so it can be re-applied after an
/// automatic reconnect.
#[derive(Debug, Clone, Copy)]
enum ChannelSel {
    /// IBonDriver v1 `SetChannel(BYTE)`.
    V1 { channel: u8 },
    /// IBonDriver v2 `SetChannel(space, channel)`.
    V2 {
        space: u32,
        channel: u32,
        priority: i32,
        exclusive: bool,
    },
}

/// Snapshot of the session state the client asked for, used to rebuild the
/// server-side context transparently after an unexpected disconnect.
#[derive(Debug, Clone, Default)]
struct SessionContext {
    /// The client currently wants a tuner open.
    tuner_open: bool,
    /// The client currently wants the TS stream running.
    streaming: bool,
    /// The last channel the client selected (if any).
    channel: Option<ChannelSel>,
}

/// Link status shared between the synchronous FFI callers and the async
/// supervisor.  Distinct from the public `ConnectionState` (which is kept at a
/// plausible "still open" value during an outage for the FFI surface): this
/// tracks whether the steady-state loop is actually pumping the socket right
/// now, so requests can fail fast instead of blocking for the full read timeout
/// while the supervisor is backing off / re-establishing.
mod link {
    /// Initial handshake in progress — requests wait normally (the initial
    /// `Hello`/`OpenTuner` legitimately need to block until the loop starts).
    pub const CONNECTING: u8 = 0;
    /// Steady-state loop is running — requests are processed normally.
    pub const UP: u8 = 1;
    /// Connection dropped; supervisor is backing off / restoring.  Requests
    /// fail fast so the synchronous FFI surface stays responsive.
    pub const DOWN: u8 = 2;
}

/// Manages the TCP connection to the proxy server.
pub struct Connection {
    /// Configuration.
    config: ConnectionConfig,
    /// Current state.
    state: Mutex<ConnectionState>,
    /// Ring buffer for TS data.
    buffer: Arc<TsRingBuffer>,
    /// Channel for sending requests (tokio mpsc — async sender from sync caller).
    request_tx: Mutex<Option<mpsc::Sender<ClientMessage>>>,
    /// Channel for receiving responses (std::sync::mpsc for blocking recv_timeout).
    /// Using std mpsc instead of tokio mpsc avoids the 1 ms poll loop in
    /// send_request_with_timeout, matching the per-command Win32 auto-reset
    /// events used by BonDriverProxy(Ex).
    response_rx: Mutex<Option<std::sync::mpsc::Receiver<ServerMessage>>>,
    /// Tokio runtime handle.
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    /// BonDriver version reported by server.
    bondriver_version: Mutex<u8>,
    /// Cached signal level and the time it was last fetched.
    /// TTL = 2 s — avoids a network round-trip on every TVTest poll.
    signal_level: Mutex<(f32, Option<std::time::Instant>)>,
    /// Session context to restore after an automatic reconnect.
    session: Mutex<SessionContext>,
    /// Set to true by `disconnect()` (explicit CloseTuner/Release path) so the
    /// reconnect supervisor knows the drop was intentional and must not retry.
    closing: AtomicBool,
    /// Wakes the supervisor out of a backoff sleep when shutting down.
    reconnect_notify: Notify,
    /// Whether the steady-state loop is currently pumping the socket (see the
    /// `link` module).  Lets `send_request_with_timeout` fail fast while the
    /// supervisor is reconnecting instead of blocking for the full timeout.
    link_status: AtomicU8,
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
            signal_level: Mutex::new((0.0, None)),
            session: Mutex::new(SessionContext::default()),
            closing: AtomicBool::new(false),
            reconnect_notify: Notify::new(),
            link_status: AtomicU8::new(link::DOWN),
        })
    }

    /// Derive the state that should be publicly visible while a connection is
    /// active or being restored, based on what the client last asked for.
    fn active_state(&self) -> ConnectionState {
        let ctx = self.session.lock();
        if ctx.streaming {
            ConnectionState::Streaming
        } else if ctx.tuner_open {
            ConnectionState::TunerOpen
        } else {
            ConnectionState::Connected
        }
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

    /// Get the cached signal level (no network round-trip).
    #[allow(dead_code)]
    pub fn signal_level(&self) -> f32 {
        self.signal_level.lock().0
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

    /// Test-only: wire the connection to channels that nobody services, so a
    /// control RPC blocks for the full read timeout instead of failing fast.
    /// Lets a test reproduce "a round-trip is in flight" without a server.
    ///
    /// The returned receiver must be kept alive by the caller; dropping it
    /// makes sends fail immediately.
    #[cfg(test)]
    pub(crate) fn stub_unanswered_rpc(&self) -> mpsc::Receiver<ClientMessage> {
        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(32);
        let (_resp_tx, resp_rx) = std::sync::mpsc::channel::<ServerMessage>();
        // Leak the sender so the channel stays open (a dropped sender would turn
        // the wait into an immediate Disconnected).
        std::mem::forget(_resp_tx);
        *self.request_tx.lock() = Some(req_tx);
        *self.response_rx.lock() = Some(resp_rx);
        self.link_status.store(link::UP, Ordering::SeqCst);
        req_rx
    }

    /// Connect to the server.
    ///
    /// A failed attempt must leave the connection **retryable**. Parking it in
    /// `Error` would make the instance permanently dead: `open_tuner` only
    /// connects from `Disconnected`, and only `disconnect()` (i.e. `Release`)
    /// clears `Error` — so a transient outage while the host is starting up
    /// would require reloading the driver. On a site-to-site link that is a
    /// routine occurrence, not an exceptional one.
    pub fn connect(self: &Arc<Self>) -> bool {
        file_log!(info, "Connection::connect() called");

        {
            let current = *self.state.lock();
            file_log!(debug, "connect: Current state = {:?}", current);
            match current {
                ConnectionState::Disconnected => {}
                ConnectionState::Error => {
                    // Remains of a previous failed attempt (runtime, channels,
                    // half-open supervisor). Tear them down, then retry.
                    file_log!(info, "connect: retrying after a previous failed attempt");
                    self.disconnect();
                }
                other => {
                    file_log!(
                        warn,
                        "connect: Already connected or connecting, state = {:?}",
                        other
                    );
                    return false;
                }
            }
        }

        let mut state = self.state.lock();
        if *state != ConnectionState::Disconnected {
            file_log!(warn, "connect: State changed while retrying: {:?}", *state);
            return false;
        }
        *state = ConnectionState::Connecting;
        drop(state);

        // Fresh session: clear the "closing" latch and any stale restore context
        // left over from a previous connection.
        self.closing.store(false, Ordering::SeqCst);
        // Initial handshake: allow requests to wait normally until the loop runs.
        self.link_status.store(link::CONNECTING, Ordering::SeqCst);
        *self.session.lock() = SessionContext::default();

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
                // Back to Disconnected, not Error — see `connect`'s doc comment.
                self.disconnect();
                return false;
            }
        };

        file_log!(debug, "connect: Creating channels...");
        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(32);
        // Use std::sync::mpsc for responses so the sync caller can use
        // recv_timeout() instead of spinning with sleep().
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<ServerMessage>();

        *self.request_tx.lock() = Some(req_tx);
        *self.response_rx.lock() = Some(resp_rx);

        let conn = Arc::clone(self);
        let config = self.config.clone();
        let buffer = Arc::clone(&self.buffer);

        file_log!(
            info,
            "connect: Spawning connection supervisor to {}",
            config.server_addr
        );
        runtime.spawn(async move {
            file_log!(info, "connect: Connection supervisor started");
            connection_supervisor(conn, config, req_rx, resp_tx, buffer).await;
            file_log!(info, "connect: Connection supervisor ended");
        });

        *self.runtime.lock() = Some(runtime);

        // The Hello message is queued via blocking_send into the mpsc channel immediately.
        // The connection supervisor will dequeue and send it once the TCP connection is established.
        // send_hello() polls for the HelloAck response, so no fixed sleep is needed here.
        // A small yield gives the runtime time to schedule the supervisor.
        std::thread::sleep(Duration::from_millis(10));

        // Perform handshake with timeout
        file_log!(info, "connect: Sending hello...");
        if !self.send_hello() {
            file_log!(error, "connect: Handshake failed");
            error!("Handshake failed");
            // Tear the failed attempt down so the next OpenTuner can retry
            // instead of finding the instance parked in Error forever.
            self.disconnect();
            return false;
        }

        // Send service filter preference if single-service mode is enabled
        if self.config.single_service {
            file_log!(
                info,
                "connect: Sending SetServiceFilter (single_service=true)"
            );
            let resp = self.send_request(ClientMessage::SetServiceFilter {
                single_service: true,
            });
            match resp {
                Some(ServerMessage::SetServiceFilterAck { success }) if success => {
                    file_log!(info, "connect: Service filter set to single-service mode");
                }
                _ => {
                    file_log!(warn, "connect: Server did not accept SetServiceFilter, continuing with all-service mode");
                    warn!(
                        "Server did not accept SetServiceFilter, continuing with all-service mode"
                    );
                }
            }
        }

        file_log!(info, "connect: Connected successfully");
        *self.state.lock() = ConnectionState::Connected;
        true
    }

    /// Disconnect from the server.
    ///
    /// This is the explicit-close path (Release / final teardown).  It latches
    /// `closing` so the reconnect supervisor treats the drop as intentional and
    /// does not attempt to reconnect, and wakes the supervisor if it happens to
    /// be sitting in a backoff sleep.
    pub fn disconnect(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.link_status.store(link::DOWN, Ordering::SeqCst);
        self.reconnect_notify.notify_waiters();

        // Drop the request channel to signal shutdown
        *self.request_tx.lock() = None;
        *self.response_rx.lock() = None;

        // Shutdown runtime
        if let Some(rt) = self.runtime.lock().take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }

        *self.session.lock() = SessionContext::default();
        self.buffer.clear();
        *self.state.lock() = ConnectionState::Disconnected;
    }

    /// Send a message and wait for response with timeout.
    ///
    /// Uses `std::sync::mpsc::Receiver::recv_timeout()` for a true blocking
    /// wait — no spin loop, no sleep().  This mirrors the per-command
    /// `WaitForMultipleObjects` + auto-reset event pattern in BonDriverProxy(Ex).
    ///
    /// # Keeping requests and replies paired
    ///
    /// The wire protocol carries no correlation id, so a reply is matched to a
    /// request purely by position in the response channel.  That is fine until
    /// one request times out: its reply arrives afterwards, stays queued, and
    /// the *next* request picks it up — from then on every request reads the
    /// previous request's answer and the caller sees an endless string of
    /// failures.  One slow round-trip poisons the session permanently.
    ///
    /// Two cheap guards make the pairing self-healing without a protocol
    /// change: drop anything already queued before sending (it can only belong
    /// to an abandoned request), and skip replies whose type cannot answer the
    /// request we just sent.
    fn send_request_with_timeout(
        &self,
        msg: ClientMessage,
        timeout: Duration,
    ) -> Option<ServerMessage> {
        // Fast-fail while the link is known-down (the supervisor is backing off
        // or re-establishing).  Otherwise a request would sit in the channel
        // unprocessed and block the caller — and hold the response_rx lock — for
        // the full read timeout, freezing the synchronous FFI surface during an
        // outage.  During an outage GetTsStream reads the ring buffer directly
        // and GetSignalLevel returns its cache, so returning None here keeps
        // those responsive.
        if self.link_status.load(Ordering::Acquire) == link::DOWN {
            debug!("[Connection] Link down; failing request fast");
            file_log!(
                warn,
                "RPC {} skipped: link is down (state={:?})",
                client_message_name(&msg),
                self.state()
            );
            return None;
        }

        // Hold the response channel across the whole exchange so no other
        // caller can consume our reply, and so the drain below cannot race a
        // concurrent send.
        const SLICE: Duration = Duration::from_millis(200);
        let rx = self.response_rx.lock();
        let rx = match rx.as_ref() {
            Some(rx) => rx,
            None => {
                error!("[Connection] Response channel not initialized");
                return None;
            }
        };

        // Anything queued before we send belongs to a request that already gave
        // up. Leaving it would hand it to us as if it were our own reply.
        let mut stale = 0usize;
        while rx.try_recv().is_ok() {
            stale += 1;
        }
        if stale > 0 {
            warn!(
                "[Connection] Discarded {} stale response(s) before sending",
                stale
            );
            file_log!(
                warn,
                "[Connection] Discarded {} stale response(s) before sending",
                stale
            );
        }

        // Send the request (briefly holds request_tx lock).
        {
            let tx = self.request_tx.lock();
            let tx = tx.as_ref()?;
            debug!(
                "[Connection] Sending message: {:?}",
                std::mem::discriminant(&msg)
            );
            if tx.blocking_send(msg.clone()).is_err() {
                error!("[Connection] Failed to send request to server");
                file_log!(
                    error,
                    "RPC {} failed: request channel is closed",
                    client_message_name(&msg)
                );
                return None;
            }
        }

        // Wait for a response in short slices, re-checking the link between them.
        // This still blocks the caller (mirroring the per-command wait in
        // BonDriverProxy(Ex)), but if the connection drops mid-wait we bail
        // within one slice and release the response_rx lock promptly instead of
        // pinning it — and serializing every other control call — for the full
        // timeout.
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                warn!("[Connection] Request timed out after {:?}", timeout);
                file_log!(
                    error,
                    "RPC {} timed out after {:?} (state={:?})",
                    client_message_name(&msg),
                    timeout,
                    self.state()
                );
                return None;
            }
            let wait = SLICE.min(deadline - now);
            match rx.recv_timeout(wait) {
                Ok(resp) => {
                    if !is_reply_to(&msg, &resp) {
                        // A late reply to an earlier request. Drop it and keep
                        // waiting for ours instead of returning it and leaving
                        // the channel offset by one forever.
                        warn!(
                            "[Connection] Ignoring unrelated response {:?} while waiting for a reply to {:?}",
                            std::mem::discriminant(&resp),
                            std::mem::discriminant(&msg)
                        );
                        continue;
                    }
                    if let ServerMessage::Error {
                        error_code,
                        message,
                    } = &resp
                    {
                        file_log!(
                            error,
                            "RPC {} rejected: error_code={} message={:?}",
                            client_message_name(&msg),
                            error_code,
                            message
                        );
                    }
                    debug!("[Connection] Received response");
                    return Some(resp);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Bail early if the link dropped (or is closing) while we
                    // waited, so we don't hold the lock for the full timeout.
                    if self.link_status.load(Ordering::Acquire) == link::DOWN {
                        debug!("[Connection] Link went down while waiting; failing request");
                        file_log!(
                            error,
                            "RPC {} interrupted: link dropped while waiting (state={:?})",
                            client_message_name(&msg),
                            self.state()
                        );
                        return None;
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    error!("[Connection] Response channel closed");
                    file_log!(
                        error,
                        "RPC {} failed: response channel closed",
                        client_message_name(&msg)
                    );
                    return None;
                }
            }
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
                stream_class: self.config.stream_class,
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
        // Use connect_timeout (not read_timeout) for the initial handshake.
        let resp = self.send_request_with_timeout(
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                stream_class: self.config.stream_class,
            },
            self.config.connect_timeout,
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
                    self.session.lock().tuner_open = true;
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
        // Explicit tuner close: forget the restore context so a later reconnect
        // does not silently reopen the tuner behind the app's back.
        {
            let mut ctx = self.session.lock();
            ctx.tuner_open = false;
            ctx.streaming = false;
            ctx.channel = None;
        }
        *self.state.lock() = ConnectionState::Connected;
    }

    /// Drop the cached signal level so the next `GetSignalLevel` refetches.
    ///
    /// Must be called whenever the tuned channel changes.  The cache exists to
    /// spare a round-trip per poll, but a value measured on the *previous*
    /// channel is worse than no value at all: a channel scan (EDCB, TVTest)
    /// reads the level right after `SetChannel` to decide whether the channel
    /// exists, so a stale reading makes it record dead channels as present and
    /// live ones as absent — the client-side twin of the server-side
    /// "FE_READ_STATUS returns the previous channel's lock" bug.
    fn invalidate_signal_cache(&self) {
        *self.signal_level.lock() = (0.0, None);
    }

    /// Set channel (IBonDriver v1).
    pub fn set_channel(&self, channel: u8, _force: bool) -> bool {
        // Invalidate before the request: the tuner starts moving as soon as the
        // server acts on it, so anything cached from here on describes the old
        // channel regardless of whether the ack ends up succeeding.
        self.invalidate_signal_cache();

        let resp = self.send_request(ClientMessage::SetChannel {
            channel,
            priority: self.config.client_priority,
            exclusive: self.config.client_exclusive,
        });

        match resp {
            Some(ServerMessage::SetChannelAck { success, .. }) => {
                if success {
                    self.session.lock().channel = Some(ChannelSel::V1 { channel });
                }
                success
            }
            _ => false,
        }
    }

    /// Set channel by space (IBonDriver v2).
    pub fn set_channel_space(
        &self,
        space: u32,
        channel: u32,
        priority: i32,
        exclusive: bool,
    ) -> bool {
        self.invalidate_signal_cache();

        let resp = self.send_request(ClientMessage::SetChannelSpace {
            space,
            channel,
            priority,
            exclusive,
        });

        match resp {
            Some(ServerMessage::SetChannelSpaceAck { success, .. }) => {
                if success {
                    self.session.lock().channel = Some(ChannelSel::V2 {
                        space,
                        channel,
                        priority,
                        exclusive,
                    });
                } else {
                    file_log!(
                        error,
                        "SetChannelSpace rejected: space={} channel={} priority={} exclusive={}",
                        space,
                        channel,
                        priority,
                        exclusive
                    );
                }
                success
            }
            Some(other) => {
                file_log!(
                    error,
                    "SetChannelSpace unexpected response: space={} channel={} response={:?}",
                    space,
                    channel,
                    std::mem::discriminant(&other)
                );
                false
            }
            None => {
                file_log!(
                    error,
                    "SetChannelSpace failed without response: space={} channel={} state={:?}",
                    space,
                    channel,
                    self.state()
                );
                false
            }
        }
    }

    /// Get signal level with a 2-second TTL cache.
    ///
    /// BonDriverProxy(Ex) updates signal level once per second inside the
    /// TsReader thread; clients read it locally with no network cost.
    /// We approximate this by caching the value for 2 s and only making a
    /// network round-trip when the cache expires.
    pub fn get_signal_level(&self) -> f32 {
        const TTL: Duration = Duration::from_secs(2);

        // Return cached value if still fresh.
        {
            let cache = self.signal_level.lock();
            if let Some(fetched_at) = cache.1 {
                if fetched_at.elapsed() < TTL {
                    return cache.0;
                }
            }
        }

        // Cache expired — fetch from server.
        let resp = self.send_request(ClientMessage::GetSignalLevel);
        match resp {
            Some(ServerMessage::GetSignalLevelAck { signal_level }) => {
                *self.signal_level.lock() = (signal_level, Some(std::time::Instant::now()));
                signal_level
            }
            _ => self.signal_level.lock().0,
        }
    }

    /// Start streaming.
    pub fn start_stream(&self) -> bool {
        if self.state() != ConnectionState::TunerOpen {
            file_log!(
                error,
                "StartStream skipped: invalid client state {:?}",
                self.state()
            );
            return false;
        }

        let resp = self.send_request(ClientMessage::StartStream);

        match resp {
            Some(ServerMessage::StartStreamAck { success, .. }) => {
                if success {
                    *self.state.lock() = ConnectionState::Streaming;
                    self.session.lock().streaming = true;
                }
                if !success {
                    file_log!(error, "StartStream rejected by server");
                }
                success
            }
            Some(other) => {
                file_log!(
                    error,
                    "StartStream unexpected response: {:?}",
                    std::mem::discriminant(&other)
                );
                false
            }
            None => {
                file_log!(
                    error,
                    "StartStream failed without response: client_state={:?}",
                    self.state()
                );
                false
            }
        }
    }

    /// Stop streaming.
    pub fn stop_stream(&self) {
        if self.state() != ConnectionState::Streaming {
            return;
        }

        let _ = self.send_request(ClientMessage::StopStream);
        self.session.lock().streaming = false;
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

// =============================================================================
// Reconnect supervisor
// =============================================================================

/// Boxed read/write halves so the plain-TCP and TLS transports share one type
/// and can be swapped on every reconnect attempt without duplicating the loop.
type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Why the steady-state connection loop returned.
enum LoopExit {
    /// All request senders were dropped — the client explicitly closed
    /// (Release / disconnect).  Do not reconnect.
    Shutdown,
    /// The connection dropped unexpectedly (EOF or IO error).  Reconnect if the
    /// client still wants to be connected.
    Dropped(String),
}

/// Initial backoff before the first reconnect attempt.
const BACKOFF_INITIAL_MS: u64 = 500;
/// Upper bound on the backoff delay.
const BACKOFF_MAX_MS: u64 = 30_000;

/// Exponential backoff schedule (pure, deterministic — the jitter is added by
/// the caller).  `attempt` is 0-based: 0 → 500 ms, 1 → 1 s, 2 → 2 s, … capped
/// at 30 s.  Monotonically non-decreasing and saturating.
fn backoff_delay(attempt: u32) -> Duration {
    // Shift by at most 20 to avoid overflow; anything past the cap clamps anyway.
    let scaled = BACKOFF_INITIAL_MS.saturating_mul(1u64 << attempt.min(20));
    Duration::from_millis(scaled.min(BACKOFF_MAX_MS))
}

/// Add a small (<= 20%) jitter to a backoff delay to avoid a thundering herd of
/// clients reconnecting to a restarted server in lockstep.  Uses wall-clock
/// nanoseconds as a cheap entropy source (no `rand` dependency).
fn jittered(base: Duration) -> Duration {
    let span_ms = (base.as_millis() as u64) / 5;
    if span_ms == 0 {
        return base;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base + Duration::from_millis(nanos % (span_ms + 1))
}

/// Sleep for `delay`, but wake early (returning `true`) if the client requests
/// shutdown while we wait.
async fn backoff_wait(conn: &Connection, delay: Duration) -> bool {
    if conn.closing.load(Ordering::SeqCst) {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => conn.closing.load(Ordering::SeqCst),
        _ = conn.reconnect_notify.notified() => true,
    }
}

/// Idle time after which TCP starts probing a quiet connection.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
/// Interval between keepalive probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Number of unanswered probes before the connection is declared dead.
#[cfg(not(windows))]
const KEEPALIVE_RETRIES: u32 = 3;

/// How long the stream may stay silent before we treat the link as dead.
///
/// TCP keepalive alone is not enough: a path can stay "up" while the server has
/// stopped producing, and on some NAT devices probes are answered by the
/// middlebox rather than the peer. A tuner that is streaming produces data
/// continuously, so silence this long means something is wrong regardless of
/// what TCP believes. Only armed while the client actually expects TS.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the *first* TS chunk after StartStream or a channel
/// switch, before `STREAM_IDLE_TIMEOUT` takes over.
///
/// Startup is not the steady state: the server may still be opening the
/// BonDriver, waiting for a lock, or — for BS4K (`stream_format = 'mmttlv'`)
/// — spawning the external MMT/TLV converter, which alone can take around 20
/// seconds before a single byte of TS appears. Policing that window with the
/// steady-state timeout drops the link exactly when the stream is about to
/// start, and the reconnect restarts the same slow startup from scratch —
/// a loop that never converges. Once TS has been seen, silence really does
/// mean a dead link, so the short timeout applies from then on.
const FIRST_DATA_GRACE: Duration = Duration::from_secs(60);

/// Turn on TCP keepalive.
///
/// Without it a silently blackholed path (NAT session expiry, route withdrawal)
/// never produces FIN or RST, so the read loop waits forever, no error surfaces
/// and the reconnect supervisor is never triggered: the stream stops and never
/// comes back. Site-to-site links do this routinely.
///
/// Best-effort — a failure here degrades to the previous behaviour and must not
/// abort an otherwise usable connection.
fn configure_keepalive(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    // Windows derives the probe count from the other two values.
    #[cfg(not(windows))]
    let keepalive = keepalive.with_retries(KEEPALIVE_RETRIES);

    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        warn!("Failed to enable TCP keepalive: {}", e);
        file_log!(warn, "Failed to enable TCP keepalive: {}", e);
    }
}

/// Establish a fresh transport (TCP, optionally wrapped in TLS) and return its
/// boxed read/write halves.
async fn establish(
    config: &ConnectionConfig,
) -> Result<(BoxReader, BoxWriter), Box<dyn std::error::Error + Send + Sync>> {
    file_log!(
        debug,
        "establish: TCP connect to {} (timeout {:?})",
        config.server_addr,
        config.connect_timeout
    );
    let stream = tokio::time::timeout(
        config.connect_timeout,
        TcpStream::connect(&config.server_addr),
    )
    .await??;
    stream.set_nodelay(true)?;
    configure_keepalive(&stream);
    info!("Connected to {}", config.server_addr);

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
            return Ok((Box::new(reader), Box::new(writer)));
        }
    }

    let (reader, writer) = stream.into_split();
    Ok((Box::new(reader), Box::new(writer)))
}

/// Decode every complete frame currently buffered in `read_buf`.
///
/// TS data is written straight into the ring buffer (single copy).  Every other
/// (control) message is handed to `on_msg`.  When `stop_after_first_control` is
/// true the function returns as soon as it has emitted one control message,
/// leaving any remaining bytes in `read_buf` untouched — this is what the
/// handshake/restore path uses to read one ack at a time without consuming
/// subsequent frames.
///
/// Returns whether at least one `TsData` frame was seen, which is what the
/// connection loop uses to tell "the stream has started" from "the server is
/// still only answering commands" (see `FIRST_DATA_GRACE`).
fn process_frames(
    read_buf: &mut BytesMut,
    buffer: &TsRingBuffer,
    mut on_msg: impl FnMut(ServerMessage),
    stop_after_first_control: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut ts_seen = false;
    static TS_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static TS_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    while read_buf.len() >= HEADER_SIZE {
        match decode_header(read_buf)? {
            Some(header) => {
                let total_len = HEADER_SIZE + header.payload_len as usize;
                if read_buf.len() < total_len {
                    break; // Need more data
                }

                // Consume header bytes.
                let _ = read_buf.split_to(HEADER_SIZE);

                // --- TsData fast path (single copy into the ring buffer) ---
                if header.message_type == MessageType::TsData {
                    let ts_payload = read_buf.split_to(header.payload_len as usize);
                    ts_seen = true;

                    let count = TS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    TS_BYTES.fetch_add(
                        ts_payload.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );

                    let written = buffer.write(&ts_payload);

                    if count % 100 == 0 {
                        let total_bytes = TS_BYTES.load(std::sync::atomic::Ordering::Relaxed);
                        crate::file_log!(
                            info,
                            "TsData #{}: {} bytes, written={}, buffer={}, total={}",
                            count,
                            ts_payload.len(),
                            written,
                            buffer.available(),
                            total_bytes
                        );
                    }

                    if written < ts_payload.len() {
                        crate::file_log!(
                            warn,
                            "Buffer full, dropped {} bytes",
                            ts_payload.len() - written
                        );
                    }

                    continue;
                }

                // --- Control messages ---
                // freeze() is zero-copy (BytesMut → Bytes without cloning).
                let payload = read_buf.split_to(header.payload_len as usize).freeze();
                let msg = decode_server_message(header.message_type, payload)?;
                on_msg(msg);

                if stop_after_first_control {
                    return Ok(ts_seen);
                }
            }
            None => break, // Need more data
        }
    }
    Ok(ts_seen)
}

/// Read frames until a single control (non-TS) message arrives, feeding any
/// interleaved TS data into the ring buffer.  Used during reconnect handshake.
async fn read_control_message(
    reader: &mut BoxReader,
    read_buf: &mut BytesMut,
    buffer: &TsRingBuffer,
) -> Result<ServerMessage, Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let mut found: Option<ServerMessage> = None;
        process_frames(
            read_buf,
            buffer,
            |m| {
                if found.is_none() {
                    found = Some(m);
                }
            },
            true,
        )?;
        if let Some(m) = found {
            return Ok(m);
        }
        let n = reader.read_buf(read_buf).await?;
        if n == 0 {
            return Err("connection closed during handshake".into());
        }
    }
}

/// Encode and send a single client message on `writer`, flushing immediately.
async fn send_control_message(
    writer: &mut BoxWriter,
    msg: &ClientMessage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let encoded = encode_client_message(msg)?;
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Re-establish the server-side session context after an automatic reconnect so
/// streaming resumes without the host app re-opening the driver: re-send Hello,
/// re-apply the service filter, and (if a tuner/channel/stream was active) then
/// OpenTuner → SetChannel → StartStream to the last known selection.
async fn restore_session(
    conn: &Connection,
    config: &ConnectionConfig,
    reader: &mut BoxReader,
    writer: &mut BoxWriter,
    buffer: &TsRingBuffer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Drop whatever the ring buffer still holds from before the outage.
    // Keeping it would splice pre-drop TS directly onto post-restore TS, and
    // the consumer sees a continuity break at the seam — a burst of Drop/Error
    // on every reconnect. On a link that flaps, that is most of the errors the
    // viewer reports. The server restarts the stream from scratch anyway, so
    // nothing here is worth preserving.
    buffer.clear();

    let mut buf = BytesMut::with_capacity(262144);

    // --- Hello ---
    send_control_message(
        writer,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            stream_class: config.stream_class,
        },
    )
    .await?;
    match read_control_message(reader, &mut buf, buffer).await? {
        ServerMessage::HelloAck { success: true, .. } => {}
        ServerMessage::HelloAck { success: false, .. } => {
            return Err("server rejected Hello on reconnect".into())
        }
        other => {
            return Err(format!(
                "unexpected reply to Hello: {:?}",
                std::mem::discriminant(&other)
            )
            .into())
        }
    }

    // --- Service filter (best effort, mirrors initial connect) ---
    if config.single_service {
        send_control_message(
            writer,
            &ClientMessage::SetServiceFilter {
                single_service: true,
            },
        )
        .await?;
        let _ = read_control_message(reader, &mut buf, buffer).await?;
    }

    // Snapshot what the client wants restored.
    let ctx = conn.session.lock().clone();

    // --- OpenTuner ---
    if ctx.tuner_open {
        send_control_message(
            writer,
            &ClientMessage::OpenTuner {
                tuner_path: config.tuner_path.clone(),
            },
        )
        .await?;
        match read_control_message(reader, &mut buf, buffer).await? {
            ServerMessage::OpenTunerAck {
                success: true,
                bondriver_version,
                ..
            } => {
                *conn.bondriver_version.lock() = bondriver_version;
            }
            _ => return Err("server rejected OpenTuner on reconnect".into()),
        }
    }

    // --- SetChannel ---
    if let Some(sel) = ctx.channel {
        match sel {
            ChannelSel::V1 { channel } => {
                send_control_message(
                    writer,
                    &ClientMessage::SetChannel {
                        channel,
                        priority: config.client_priority,
                        exclusive: config.client_exclusive,
                    },
                )
                .await?;
                match read_control_message(reader, &mut buf, buffer).await? {
                    ServerMessage::SetChannelAck { success: true, .. } => {}
                    _ => return Err("server rejected SetChannel on reconnect".into()),
                }
            }
            ChannelSel::V2 {
                space,
                channel,
                priority,
                exclusive,
            } => {
                send_control_message(
                    writer,
                    &ClientMessage::SetChannelSpace {
                        space,
                        channel,
                        priority,
                        exclusive,
                    },
                )
                .await?;
                match read_control_message(reader, &mut buf, buffer).await? {
                    ServerMessage::SetChannelSpaceAck { success: true, .. } => {}
                    _ => return Err("server rejected SetChannelSpace on reconnect".into()),
                }
            }
        }
    }

    // --- StartStream ---
    if ctx.streaming {
        send_control_message(writer, &ClientMessage::StartStream).await?;
        match read_control_message(reader, &mut buf, buffer).await? {
            ServerMessage::StartStreamAck { success: true, .. } => {}
            _ => return Err("server rejected StartStream on reconnect".into()),
        }
    }

    // Any bytes read past the last ack (e.g. leading TS packets) were already
    // routed into the ring buffer by process_frames, so nothing is lost when we
    // hand off to the steady-state loop with a fresh read buffer.
    Ok(())
}

/// Supervisor: owns the request receiver / response sender for the whole life
/// of the connection and reuses the single tokio runtime across reconnects.
///
/// The first iteration drives the initial handshake through the request/response
/// channels exactly as before (`connect()` calls `send_hello()` etc. from the
/// synchronous side).  Subsequent iterations restore the session inline before
/// resuming the steady-state loop.
async fn connection_supervisor(
    conn: Arc<Connection>,
    config: ConnectionConfig,
    mut req_rx: mpsc::Receiver<ClientMessage>,
    resp_tx: std::sync::mpsc::Sender<ServerMessage>,
    buffer: Arc<TsRingBuffer>,
) {
    let mut attempt: u32 = 0;
    let mut first = true;

    loop {
        if conn.closing.load(Ordering::SeqCst) {
            break;
        }

        match establish(&config).await {
            Ok((mut reader, mut writer)) => {
                if first {
                    file_log!(info, "supervisor: initial connection established");
                } else {
                    // Reconnected: rebuild server-side context before resuming.
                    match restore_session(&conn, &config, &mut reader, &mut writer, &buffer).await {
                        Ok(()) => {
                            let st = conn.active_state();
                            *conn.state.lock() = st;
                            attempt = 0;
                            info!("Reconnected; session restored, state={:?}", st);
                            file_log!(
                                info,
                                "supervisor: reconnected, session restored, state={:?}",
                                st
                            );
                            // Drop any pre-drop leftovers still queued from
                            // before the outage so we do not replay stale
                            // commands or desync the response channel.  This runs
                            // while the link is still DOWN (below we flip it UP),
                            // and fast-fail means callers didn't enqueue new
                            // requests during the outage — so nothing issued in
                            // the reconnect window is silently swallowed here.
                            while req_rx.try_recv().is_ok() {}
                        }
                        Err(e) => {
                            warn!("Session restore failed: {}", e);
                            file_log!(warn, "supervisor: session restore failed: {}", e);
                            let delay = jittered(backoff_delay(attempt));
                            attempt = attempt.saturating_add(1);
                            if backoff_wait(&conn, delay).await {
                                break;
                            }
                            continue;
                        }
                    }
                }
                first = false;

                // Link is now pumping the socket: requests are processed normally.
                conn.link_status.store(link::UP, Ordering::SeqCst);

                match connection_loop(&conn, &mut req_rx, &resp_tx, &buffer, reader, writer).await {
                    LoopExit::Shutdown => {
                        conn.link_status.store(link::DOWN, Ordering::SeqCst);
                        info!("Connection closed by client");
                        file_log!(info, "supervisor: client requested shutdown");
                        break;
                    }
                    LoopExit::Dropped(reason) => {
                        // Link is down: fail synchronous requests fast until we
                        // are back up, so the FFI surface stays responsive.
                        conn.link_status.store(link::DOWN, Ordering::SeqCst);
                        if conn.closing.load(Ordering::SeqCst) {
                            break;
                        }
                        warn!("Connection dropped ({}); will reconnect", reason);
                        file_log!(
                            warn,
                            "supervisor: connection dropped ({}); reconnecting",
                            reason
                        );
                        // Keep the public state at the last active state (so
                        // IsTunerOpening/GetActiveDeviceNum stay truthy while a
                        // tuner is open); use the transient Reconnecting state
                        // only when nothing was open.
                        let st = conn.active_state();
                        *conn.state.lock() = if st == ConnectionState::Connected {
                            ConnectionState::Reconnecting
                        } else {
                            st
                        };
                        let delay = jittered(backoff_delay(attempt));
                        attempt = attempt.saturating_add(1);
                        if backoff_wait(&conn, delay).await {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                // Establishing failed: link is down, fail requests fast (this
                // also unblocks the initial send_hello promptly on first-connect
                // failure instead of making it wait the full connect timeout).
                conn.link_status.store(link::DOWN, Ordering::SeqCst);
                if first {
                    // Initial connect failed: report the error and stop.  The
                    // synchronous connect() observes this and returns failure.
                    error!("Initial connection failed: {}", e);
                    file_log!(error, "supervisor: initial connection failed: {}", e);
                    *conn.state.lock() = ConnectionState::Error;
                    break;
                }
                if conn.closing.load(Ordering::SeqCst) {
                    break;
                }
                warn!("Reconnect attempt failed: {}", e);
                file_log!(warn, "supervisor: reconnect attempt failed: {}", e);
                let st = conn.active_state();
                *conn.state.lock() = if st == ConnectionState::Connected {
                    ConnectionState::Reconnecting
                } else {
                    st
                };
                let delay = jittered(backoff_delay(attempt));
                attempt = attempt.saturating_add(1);
                if backoff_wait(&conn, delay).await {
                    break;
                }
            }
        }
    }

    file_log!(info, "supervisor: exiting");
}

/// Why the writer task ended.
enum WriterExit {
    /// All senders were dropped (the loop is tearing down / client shutdown).
    ChannelClosed,
    /// The message could not be encoded.
    Encode(String),
    /// A socket write/flush failed.
    Io(String),
}

/// Steady-state connection loop.
///
/// The reader and the writer run as **independent** halves so that an outgoing
/// `write_all` that parks on TCP backpressure never stalls TS reception:
///
/// * A dedicated writer task owns the socket write half and drains an internal
///   *unbounded* channel.  Because the channel is unbounded, handing a control
///   message to it from the reader side is a synchronous, non-blocking `send`
///   — the reader loop never `.await`s on a write and so is never starved.
/// * The reader loop only reads frames and forwards decoded control messages to
///   `resp_tx`, plus non-blockingly forwards outgoing requests to the writer.
///
/// The internal channel + task are created fresh per call and torn down before
/// returning (drop the sender, then abort+join the task), so the single tokio
/// runtime is reused across reconnects with no leaked task.  `req_rx` stays
/// borrowed by the supervisor so the request channel survives reconnects.
async fn connection_loop(
    conn: &Connection,
    req_rx: &mut mpsc::Receiver<ClientMessage>,
    resp_tx: &std::sync::mpsc::Sender<ServerMessage>,
    buffer: &TsRingBuffer,
    mut reader: BoxReader,
    writer: BoxWriter,
) -> LoopExit {
    // Internal channel feeding the independent writer task.  Unbounded so the
    // reader side never blocks handing off a control message.
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let mut writer_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = write_rx.recv().await {
            let encoded = match encode_client_message(&msg) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to encode client message: {}", e);
                    return WriterExit::Encode(e.to_string());
                }
            };
            if let Err(e) = writer.write_all(&encoded).await {
                error!("Write error: {}", e);
                return WriterExit::Io(e.to_string());
            }
            // Flush after every command so it reaches the server promptly.
            if let Err(e) = writer.flush().await {
                error!("Flush error: {}", e);
                return WriterExit::Io(e.to_string());
            }
        }
        WriterExit::ChannelClosed
    });

    // Larger read buffer (256 KB) to reduce syscalls on high-bitrate streams,
    // similar to TsPacketBufSize in BonDriverProxy(Ex).
    let mut read_buf = BytesMut::with_capacity(262144);

    // `writer_done` guards against polling/awaiting the JoinHandle twice: once
    // the writer branch of the select fires, the handle is already resolved.
    let mut writer_done = false;
    let mut last_request = "none";
    let mut idle_deadline = tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT;
    // Startup (and every channel switch) gets `FIRST_DATA_GRACE` instead of
    // `STREAM_IDLE_TIMEOUT` until the first TS chunk lands — see the constant.
    let mut awaiting_first_data = false;
    let mut was_streaming = false;
    let exit = loop {
        // Only police silence while the client actually expects TS. An idle
        // tuner legitimately sends nothing for minutes at a time; TCP keepalive
        // covers that case instead.
        let expecting_data = conn.session.lock().streaming;
        if expecting_data && !was_streaming {
            // StartStream just took effect: the server may still be opening
            // the driver / starting a converter.
            awaiting_first_data = true;
            idle_deadline = tokio::time::Instant::now() + FIRST_DATA_GRACE;
        }
        was_streaming = expecting_data;

        tokio::select! {
            // --- Outgoing requests: hand off to the writer task, non-blocking. ---
            maybe_msg = req_rx.recv() => {
                match maybe_msg {
                    Some(msg) => {
                        last_request = client_message_name(&msg);
                        // A channel switch restarts the whole startup path on
                        // the server (possibly onto a 4K driver), so the
                        // stream goes quiet again for as long as a fresh
                        // StartStream would.
                        if matches!(
                            msg,
                            ClientMessage::SetChannel { .. }
                                | ClientMessage::SetChannelSpace { .. }
                                | ClientMessage::SelectLogicalChannel { .. }
                        ) {
                            awaiting_first_data = true;
                            idle_deadline = tokio::time::Instant::now() + FIRST_DATA_GRACE;
                        }
                        file_log!(debug, "connection_loop: sending request {}", last_request);
                        // Unbounded send is synchronous — never stalls the reader.
                        if write_tx.send(msg).is_err() {
                            // Writer task is gone; the socket is unusable.
                            break LoopExit::Dropped("writer task ended".to_string());
                        }
                    }
                    None => {
                        // All request senders dropped — explicit client shutdown.
                        break LoopExit::Shutdown;
                    }
                }
            }

            // --- Writer task ended (socket write error / encode failure). ---
            wres = &mut writer_handle => {
                writer_done = true;
                let reason = match wres {
                    Ok(WriterExit::Io(s)) => format!("write error: {}", s),
                    Ok(WriterExit::Encode(s)) => format!("encode error: {}", s),
                    Ok(WriterExit::ChannelClosed) => "writer channel closed".to_string(),
                    Err(e) => format!("writer task panicked: {}", e),
                };
                break LoopExit::Dropped(reason);
            }

            // --- Streaming went silent: treat as a dead link ---
            _ = tokio::time::sleep_until(idle_deadline), if expecting_data => {
                let limit = if awaiting_first_data { FIRST_DATA_GRACE } else { STREAM_IDLE_TIMEOUT };
                warn!("No TS data for {:?} while streaming; treating the link as dead", limit);
                file_log!(warn, "No TS data for {:?} while streaming; reconnecting", limit);
                break LoopExit::Dropped(format!("idle for {:?} while streaming", limit));
            }

            // --- Incoming frames ---
            res = reader.read_buf(&mut read_buf) => {
                match res {
                    Ok(0) => {
                        info!("Connection closed by server");
                        file_log!(error, "connection_loop: server closed connection (last_request={}, client_state={:?})", last_request, conn.state());
                        break LoopExit::Dropped("server closed (EOF)".to_string());
                    }
                    Ok(_) => {
                        let r = process_frames(&mut read_buf, buffer, |m| {
                            if resp_tx.send(m).is_err() {
                                debug!("Response channel closed");
                            }
                        }, false);
                        if matches!(r, Ok(true)) {
                            // Real TS: startup is over, police silence with
                            // the steady-state timeout from here on.
                            awaiting_first_data = false;
                        }
                        if !awaiting_first_data {
                            idle_deadline = tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT;
                        }
                        // While awaiting the first chunk the original
                        // `FIRST_DATA_GRACE` deadline stands: control frames
                        // (acks, signal levels) must not extend it forever.
                        if let Err(e) = r {
                            error!("Frame decode error: {}", e);
                            break LoopExit::Dropped(format!("decode error: {}", e));
                        }
                    }
                    Err(e) => {
                        error!("Read error: {}", e);
                        break LoopExit::Dropped(format!("read error: {}", e));
                    }
                }
            }
        }
    };

    // Tear the writer down deterministically: dropping the sender lets it end
    // cleanly if it is idle; the abort guarantees it cannot hang on a dead
    // socket's write.  Either way we join it so no task is leaked.
    drop(write_tx);
    if !writer_done {
        writer_handle.abort();
        let _ = writer_handle.await;
    }

    exit
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
fn build_tls_config(
    ca_cert_path: Option<&str>,
) -> Result<rustls::ClientConfig, Box<dyn std::error::Error + Send + Sync>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Data buffered before an outage must not be spliced onto the stream that
    /// comes back after it: the seam is a continuity break the viewer counts as
    /// Drop/Error, on every single reconnect.
    #[tokio::test]
    async fn reconnecting_discards_pre_outage_ts() {
        let conn = Connection::new(ConnectionConfig::default());
        let buffer = TsRingBuffer::new();

        // TS that arrived just before the link went down.
        buffer.write(&vec![0x47u8; 188 * 4]);
        assert!(!buffer.is_empty());

        // The peer never answers the Hello, so restore fails — but the buffer
        // must already have been cleared by then, since the point is that the
        // pre-outage bytes never reach the consumer either way.
        let (client_end, _server_end) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_end);
        let mut reader: BoxReader = Box::new(reader);
        let mut writer: BoxWriter = Box::new(writer);

        let config = conn.config.clone();
        let restore = restore_session(&conn, &config, &mut reader, &mut writer, &buffer);
        let _ = tokio::time::timeout(Duration::from_millis(50), restore).await;

        assert!(
            buffer.is_empty(),
            "pre-outage TS must be discarded on reconnect"
        );
    }

    /// Drive `connection_loop` against a transport that never delivers a byte.
    /// Drive `connection_loop` against a transport that never delivers a byte.
    ///
    /// The peer end of the duplex is kept alive, so this is exactly the
    /// blackhole case: no FIN, no RST, no data.
    async fn run_loop_against_a_silent_peer(streaming: bool) -> LoopExit {
        let conn = Connection::new(ConnectionConfig::default());
        conn.session.lock().streaming = streaming;

        let (client_end, _server_end) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_end);
        // Keep the request sender alive, otherwise the loop exits as Shutdown.
        let (_req_tx, mut req_rx) = mpsc::channel::<ClientMessage>(4);
        let (resp_tx, _resp_rx) = std::sync::mpsc::channel::<ServerMessage>();
        let buffer = TsRingBuffer::new();

        connection_loop(
            &conn,
            &mut req_rx,
            &resp_tx,
            &buffer,
            Box::new(reader),
            Box::new(writer),
        )
        .await
    }

    /// A path that silently blackholes produces neither FIN nor RST, so the
    /// read loop would wait forever and the reconnect supervisor would never
    /// run: the stream stops and never returns. While streaming, silence must
    /// be treated as a dropped link.
    #[tokio::test(start_paused = true)]
    async fn a_silent_link_is_dropped_while_streaming() {
        match run_loop_against_a_silent_peer(true).await {
            LoopExit::Dropped(reason) => {
                assert!(reason.contains("idle"), "unexpected reason: {reason}");
            }
            LoopExit::Shutdown => panic!("must report a drop, not a clean shutdown"),
        }
    }

    /// Startup silence is not a dead link: a BS4K driver can take ~20 s to
    /// produce its first converted chunk, which is longer than
    /// `STREAM_IDLE_TIMEOUT`. Until the first TS frame arrives the loop must
    /// hold out for `FIRST_DATA_GRACE` instead, or the reconnect kills the
    /// stream exactly when it is about to start.
    #[tokio::test(start_paused = true)]
    async fn startup_silence_is_tolerated_until_the_first_data_grace_expires() {
        assert!(
            FIRST_DATA_GRACE > STREAM_IDLE_TIMEOUT * 2,
            "the bound below only proves anything if the grace is the longer one"
        );
        let result = tokio::time::timeout(
            STREAM_IDLE_TIMEOUT * 2,
            run_loop_against_a_silent_peer(true),
        )
        .await;
        assert!(
            result.is_err(),
            "must still be waiting for the first chunk, not reconnecting"
        );
    }

    /// An idle tuner legitimately sends nothing for minutes. The idle timer must
    /// only be armed when the client is actually expecting TS — TCP keepalive
    /// covers the quiet case.
    #[tokio::test(start_paused = true)]
    async fn a_silent_link_is_tolerated_when_not_streaming() {
        let result = tokio::time::timeout(
            STREAM_IDLE_TIMEOUT * 100,
            run_loop_against_a_silent_peer(false),
        )
        .await;
        assert!(
            result.is_err(),
            "must keep waiting rather than drop the link"
        );
    }

    /// Wire up a Connection with hand-made request/response channels so the
    /// Wire up a Connection with hand-made request/response channels so the
    /// RPC pairing can be exercised without a server.
    fn rpc_harness(
        read_timeout: Duration,
    ) -> (
        Arc<Connection>,
        mpsc::Receiver<ClientMessage>,
        std::sync::mpsc::Sender<ServerMessage>,
    ) {
        let conn = Connection::new(ConnectionConfig {
            read_timeout,
            ..ConnectionConfig::default()
        });
        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(32);
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<ServerMessage>();
        *conn.request_tx.lock() = Some(req_tx);
        *conn.response_rx.lock() = Some(resp_rx);
        conn.link_status.store(link::UP, Ordering::SeqCst);
        (conn, req_rx, resp_tx)
    }

    #[test]
    fn reply_pairing_accepts_the_matching_ack_and_any_error() {
        let req = ClientMessage::GetSignalLevel;
        assert!(is_reply_to(
            &req,
            &ServerMessage::GetSignalLevelAck { signal_level: 1.0 }
        ));
        assert!(!is_reply_to(
            &req,
            &ServerMessage::SetChannelAck {
                success: true,
                error_code: 0
            }
        ));
        // The server answers a refused request of any kind with Error.
        assert!(is_reply_to(
            &req,
            &ServerMessage::Error {
                error_code: 1,
                message: "no".to_string()
            }
        ));

        // v1 and v2 SetChannel have distinct acks and must not cross over.
        let v1 = ClientMessage::SetChannel {
            channel: 1,
            priority: 0,
            exclusive: false,
        };
        let v2 = ClientMessage::SetChannelSpace {
            space: 0,
            channel: 1,
            priority: 0,
            exclusive: false,
        };
        assert!(is_reply_to(
            &v1,
            &ServerMessage::SetChannelAck {
                success: true,
                error_code: 0
            }
        ));
        assert!(!is_reply_to(
            &v1,
            &ServerMessage::SetChannelSpaceAck {
                success: true,
                error_code: 0
            }
        ));
        assert!(is_reply_to(
            &v2,
            &ServerMessage::SetChannelSpaceAck {
                success: true,
                error_code: 0
            }
        ));
        assert!(!is_reply_to(
            &v2,
            &ServerMessage::SetChannelAck {
                success: true,
                error_code: 0
            }
        ));
    }

    /// A reply that arrived after its request gave up must not be handed to the
    /// next request — otherwise the channel stays offset by one for the rest of
    /// the session and every later request reads the previous one's answer.
    #[test]
    fn a_stale_queued_reply_does_not_answer_the_next_request() {
        let (conn, mut req_rx, resp_tx) = rpc_harness(Duration::from_millis(500));

        // Left over from a SetChannel that already timed out.
        resp_tx
            .send(ServerMessage::SetChannelSpaceAck {
                success: true,
                error_code: 0,
            })
            .unwrap();

        let sender = resp_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = sender.send(ServerMessage::GetSignalLevelAck { signal_level: 12.5 });
        });

        let resp = conn.send_request(ClientMessage::GetSignalLevel);
        assert_eq!(
            resp,
            Some(ServerMessage::GetSignalLevelAck { signal_level: 12.5 }),
            "must wait for its own reply, not consume the stale one"
        );
        assert!(req_rx.try_recv().is_ok(), "the request was actually sent");
    }

    /// Same hazard, but the late reply lands while we are already waiting.
    #[test]
    fn an_unrelated_reply_arriving_mid_wait_is_skipped() {
        let (conn, _req_rx, resp_tx) = rpc_harness(Duration::from_millis(500));

        let sender = resp_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let _ = sender.send(ServerMessage::StopStreamAck { success: true });
            std::thread::sleep(Duration::from_millis(10));
            let _ = sender.send(ServerMessage::EnumTuningSpaceAck {
                name: Some("関東".to_string()),
            });
        });

        let resp = conn.send_request(ClientMessage::EnumTuningSpace { space: 0 });
        assert_eq!(
            resp,
            Some(ServerMessage::EnumTuningSpaceAck {
                name: Some("関東".to_string())
            })
        );
    }

    /// A connection attempt that fails must leave the instance ready to try
    /// again. Parking it in `Error` made the object dead until the host
    /// reloaded the driver — a transient outage at startup should not cost a
    /// TVTest restart.
    #[test]
    fn a_failed_connect_leaves_the_instance_retryable() {
        let conn = Connection::new(ConnectionConfig {
            // Nothing listening: port 1 refuses immediately on every platform
            // we support, so this stays fast.
            server_addr: "127.0.0.1:1".to_string(),
            connect_timeout: Duration::from_millis(300),
            read_timeout: Duration::from_millis(300),
            ..ConnectionConfig::default()
        });

        assert!(!conn.connect(), "connect must report failure");
        assert_eq!(
            conn.state(),
            ConnectionState::Disconnected,
            "a failed attempt must not park the instance in Error"
        );

        // The second call must actually attempt again rather than short-circuit
        // on a leftover state.
        assert!(!conn.connect());
        assert_eq!(conn.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn backoff_starts_at_initial() {
        assert_eq!(backoff_delay(0), Duration::from_millis(BACKOFF_INITIAL_MS));
    }

    #[test]
    fn backoff_is_exponential_then_capped() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_millis(1000));
        assert_eq!(backoff_delay(2), Duration::from_millis(2000));
        assert_eq!(backoff_delay(3), Duration::from_millis(4000));
        assert_eq!(backoff_delay(4), Duration::from_millis(8000));
        assert_eq!(backoff_delay(5), Duration::from_millis(16000));
        // attempt 6 would be 32 s → clamped to the 30 s cap.
        assert_eq!(backoff_delay(6), Duration::from_millis(BACKOFF_MAX_MS));
    }

    #[test]
    fn backoff_is_monotonic_and_capped_for_all_attempts() {
        let cap = Duration::from_millis(BACKOFF_MAX_MS);
        let mut prev = Duration::ZERO;
        for attempt in 0..64u32 {
            let d = backoff_delay(attempt);
            assert!(
                d >= prev,
                "backoff must be non-decreasing at attempt {}",
                attempt
            );
            assert!(
                d <= cap,
                "backoff must never exceed the cap at attempt {}",
                attempt
            );
            prev = d;
        }
        // Large attempt numbers must not panic (overflow) and stay clamped.
        assert_eq!(backoff_delay(u32::MAX), cap);
    }

    #[test]
    fn changing_channel_invalidates_the_signal_cache() {
        // A scan reads the level straight after SetChannel to decide whether
        // the channel exists. Serving the previous channel's reading out of the
        // 2 s cache makes the scan result wrong in both directions.
        let conn = Connection::new(ConnectionConfig::default());
        *conn.signal_level.lock() = (24.5, Some(Instant::now()));
        assert_eq!(conn.signal_level(), 24.5, "cache primed");

        conn.invalidate_signal_cache();
        assert!(
            conn.signal_level.lock().1.is_none(),
            "cache must be empty so the next read refetches"
        );

        // The public entry points must do it too. There is no server here, so
        // the request fails fast (link is DOWN) — the cache state is what
        // matters.
        *conn.signal_level.lock() = (24.5, Some(Instant::now()));
        conn.set_channel_space(0, 1, 0, false);
        assert!(
            conn.signal_level.lock().1.is_none(),
            "SetChannel2 must invalidate"
        );

        *conn.signal_level.lock() = (24.5, Some(Instant::now()));
        conn.set_channel(7, false);
        assert!(
            conn.signal_level.lock().1.is_none(),
            "SetChannel must invalidate"
        );
    }

    #[test]
    fn jitter_stays_within_20_percent_upper_bound() {
        let base = Duration::from_millis(1000);
        // Jitter is additive and bounded by 20% of the base.
        for _ in 0..1000 {
            let j = jittered(base);
            assert!(j >= base);
            assert!(j <= base + Duration::from_millis(200));
        }
        // A base too small to jitter is returned unchanged.
        assert_eq!(jittered(Duration::from_millis(4)), Duration::from_millis(4));
    }
}
