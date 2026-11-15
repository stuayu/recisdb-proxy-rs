//! B-CAS card transport provided by DriverHost_PX4 on macOS.
//!
//! `px4rec` uses this fixed-size local socket protocol when the native PC/SC
//! service does not expose the PX4 device's built-in smart-card reader. The
//! protocol layout mirrors `winusb/src/common/card_command.hpp` in px4_drv.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::boncaslink;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SHUTDOWN_STREAM_CLONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) const READER_PREFIX: &str = "PX4 DriverHost: ";
const DEFAULT_SOCKET: &str = "/tmp/px4_card_pipe.sock";
const SOCKET_ENV: &str = "PX4_CARD_SOCKET";
const PROTOCOL_VERSION: u32 = 0x0002_0001;
const MAX_READERS: usize = 16;
const MAX_READER_NAME: usize = 128;
pub(crate) const MAX_DATA_SIZE: usize = 512;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const READER_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const READER_CACHE_TTL: Duration = Duration::from_secs(2);

extern "C" {
    fn getpeereid(socket: c_int, euid: *mut u32, egid: *mut u32) -> c_int;
    fn geteuid() -> u32;
}

const CODE_GET_VERSION: u32 = 1;
const CODE_LIST_READERS: u32 = 2;
const CODE_CONNECT: u32 = 3;
const CODE_DISCONNECT: u32 = 5;
const CODE_STATUS: u32 = 6;
const CODE_TRANSMIT: u32 = 8;
const STATUS_SUCCEEDED: u32 = 1;
const ERROR_NO_CARD: u32 = 3;
const SHARE_MODE_SHARED: u32 = 1;

/// Exact `repr(C)` mirror of px4_drv's `px4::card_command::Command` on macOS,
/// where `wchar_t` is 32 bits and C++ `bool` is one byte.
#[repr(C)]
#[derive(Clone, Copy)]
struct CardCommand {
    code: u32,
    status: u32,
    error: u32,
    version: u32,
    reader_count: u32,
    reader_generation: u64,
    readers: [[u32; MAX_READER_NAME]; MAX_READERS],
    reader_name: [u32; MAX_READER_NAME],
    share_mode: u32,
    disposition: u32,
    // These mirror C++ bool fields on the wire. Keep them as bytes while
    // reading remote data: an invalid Rust `bool` representation is UB.
    card_present: u8,
    card_initialized: u8,
    atr: [u8; 33],
    atr_length: u32,
    data: [u8; MAX_DATA_SIZE],
    data_length: u32,
    data_capacity: u32,
}

impl CardCommand {
    fn new(code: u32) -> Self {
        // All-zero is the protocol's request initializer (`Command command{}`).
        let mut command: Self = unsafe { std::mem::zeroed() };
        command.code = code;
        command
    }

    fn set_reader_name(&mut self, name: &str) -> Result<(), Px4CardError> {
        let chars = name.chars().map(|c| c as u32).collect::<Vec<_>>();
        if chars.len() >= MAX_READER_NAME || chars.contains(&0) {
            return Err(Px4CardError::InvalidReaderName);
        }
        self.reader_name[..chars.len()].copy_from_slice(&chars);
        Ok(())
    }

    fn reader_name(field: &[u32; MAX_READER_NAME]) -> Option<String> {
        let end = field.iter().position(|&ch| ch == 0)?;
        field[..end].iter().map(|&ch| char::from_u32(ch)).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Px4CardError {
    Unavailable,
    NoCard,
    InvalidReaderName,
    InvalidResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Px4TransmitError {
    Card(Px4CardError),
    InsufficientBuffer(usize),
    InvalidParameter,
}

struct CardSession {
    stream: UnixStream,
    poisoned: bool,
    closed: bool,
    pending_response: Option<PendingResponse>,
}

struct PendingResponse {
    apdu: Vec<u8>,
    response: Vec<u8>,
}

struct CardSessionHandle {
    session: Mutex<CardSession>,
    shutdown_stream: UnixStream,
}

type SharedCardSession = Arc<CardSessionHandle>;
/// Map each opaque PC/SC card handle to its owning context and transport.
type CardSessionEntry = (usize, SharedCardSession);

static SESSIONS: OnceLock<Mutex<HashMap<usize, CardSessionEntry>>> = OnceLock::new();
#[derive(Default)]
struct ContextState {
    retiring: bool,
    active_operations: usize,
}

static LIVE_CONTEXTS: OnceLock<Mutex<HashMap<usize, ContextState>>> = OnceLock::new();
static CONTEXT_OPERATIONS_CHANGED: OnceLock<Condvar> = OnceLock::new();
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x5000_0000);
static READER_CACHE: OnceLock<Mutex<ReaderCache>> = OnceLock::new();

struct ContextOperation {
    context: usize,
}

pub(crate) struct ContextRelease {
    context: usize,
    registered: bool,
    finished: bool,
}

#[derive(Default)]
struct ReaderCache {
    readers: Vec<String>,
    updated_at: Option<Instant>,
    last_attempt_at: Option<Instant>,
}

fn readers_with_cache(
    cache: &Mutex<ReaderCache>,
    probe: impl FnOnce() -> Result<Vec<String>, Px4CardError>,
) -> Vec<String> {
    {
        let mut state = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let fresh = state
            .updated_at
            .is_some_and(|updated_at| now.duration_since(updated_at) < READER_CACHE_TTL);
        let recently_probed = state
            .last_attempt_at
            .is_some_and(|attempted_at| now.duration_since(attempted_at) < READER_CACHE_TTL);
        if fresh || recently_probed {
            return state.readers.clone();
        }
        state.last_attempt_at = Some(now);
    }

    // Socket I/O must not hold the cache mutex: concurrent callers can use
    // the previous snapshot while the refresh is in progress.
    match probe() {
        Ok(readers) => {
            let mut state = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.readers = readers.clone();
            state.updated_at = Some(Instant::now());
            readers
        }
        Err(_) => cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .readers
            .clone(),
    }
}

fn sessions() -> MutexGuard<'static, HashMap<usize, CardSessionEntry>> {
    SESSIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn live_contexts() -> MutexGuard<'static, HashMap<usize, ContextState>> {
    LIVE_CONTEXTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn register_context(context: usize) {
    live_contexts().entry(context).or_default();
}

fn register_session_for_context(context: usize, handle: usize, shared: SharedCardSession) -> bool {
    // Keep the registry lock order as contexts -> sessions. Release marks the
    // context retiring under this lock before taking its session snapshot.
    let contexts = live_contexts();
    if contexts.get(&context).map_or(true, |state| state.retiring) {
        return false;
    }
    let mut sessions = sessions();
    if sessions.contains_key(&handle) {
        return false;
    }
    sessions.insert(handle, (context, shared));
    true
}

fn begin_context_operation(context: usize) -> Result<ContextOperation, Px4CardError> {
    let mut contexts = live_contexts();
    let state = contexts
        .get_mut(&context)
        .ok_or(Px4CardError::Unavailable)?;
    if state.retiring {
        return Err(Px4CardError::Unavailable);
    }
    state.active_operations = state
        .active_operations
        .checked_add(1)
        .ok_or(Px4CardError::Unavailable)?;
    Ok(ContextOperation { context })
}

impl Drop for ContextOperation {
    fn drop(&mut self) {
        let mut contexts = live_contexts();
        if let Some(state) = contexts.get_mut(&self.context) {
            state.active_operations = state.active_operations.saturating_sub(1);
            if state.active_operations == 0 {
                CONTEXT_OPERATIONS_CHANGED
                    .get_or_init(Condvar::new)
                    .notify_all();
            }
        }
    }
}

fn session_operation(handle: usize) -> Result<(SharedCardSession, ContextOperation), Px4CardError> {
    let mut contexts = live_contexts();
    let sessions = sessions();
    let (context, shared) = sessions
        .get(&handle)
        .map(|(context, shared)| (*context, Arc::clone(shared)))
        .ok_or(Px4CardError::Unavailable)?;
    let state = contexts
        .get_mut(&context)
        .ok_or(Px4CardError::Unavailable)?;
    if state.retiring {
        return Err(Px4CardError::Unavailable);
    }
    state.active_operations = state
        .active_operations
        .checked_add(1)
        .ok_or(Px4CardError::Unavailable)?;
    drop(sessions);
    drop(contexts);
    Ok((shared, ContextOperation { context }))
}

fn lock_session(session: &SharedCardSession) -> MutexGuard<'_, CardSession> {
    session
        .session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clone_shutdown_stream(stream: &UnixStream) -> io::Result<UnixStream> {
    #[cfg(test)]
    if FAIL_NEXT_SHUTDOWN_STREAM_CLONE.with(|fail| fail.replace(false)) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "injected shutdown stream clone failure",
        ));
    }
    stream.try_clone()
}

fn shared_session(stream: UnixStream) -> Result<SharedCardSession, (Px4CardError, UnixStream)> {
    let shutdown_stream = match clone_shutdown_stream(&stream) {
        Ok(stream) => stream,
        Err(_) => return Err((Px4CardError::Unavailable, stream)),
    };
    Ok(Arc::new(CardSessionHandle {
        session: Mutex::new(CardSession {
            stream,
            poisoned: false,
            closed: false,
            pending_response: None,
        }),
        shutdown_stream,
    }))
}

fn card_status_error(status: &CardCommand) -> Option<Px4CardError> {
    if status.card_present == 0 {
        Some(Px4CardError::NoCard)
    } else if status.card_initialized == 0 {
        Some(Px4CardError::Unavailable)
    } else {
        None
    }
}

fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

fn peer_uid_is_trusted(peer_uid: u32, current_uid: u32) -> bool {
    peer_uid == 0 || peer_uid == current_uid
}

fn socket_peer_is_trusted(stream: &UnixStream) -> bool {
    let mut peer_uid = 0;
    let mut peer_gid = 0;
    // SAFETY: getpeereid only writes the uid/gid outputs for this live fd.
    let status = unsafe { getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    // SAFETY: geteuid has no preconditions and returns the process effective uid.
    status == 0 && peer_uid_is_trusted(peer_uid, unsafe { geteuid() })
}

fn connect_socket_with_timeout(timeout: Duration) -> Result<UnixStream, Px4CardError> {
    let stream = UnixStream::connect(socket_path()).map_err(|_| Px4CardError::Unavailable)?;
    if !socket_peer_is_trusted(&stream) {
        return Err(Px4CardError::Unavailable);
    }
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    Ok(stream)
}

fn connect_socket() -> Result<UnixStream, Px4CardError> {
    connect_socket_with_timeout(SOCKET_TIMEOUT)
}

fn exchange(stream: &mut UnixStream, command: &mut CardCommand) -> Result<(), Px4CardError> {
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (command as *const CardCommand).cast::<u8>(),
            std::mem::size_of::<CardCommand>(),
        )
    };
    stream
        .write_all(bytes)
        .map_err(|_| Px4CardError::Unavailable)?;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (command as *mut CardCommand).cast::<u8>(),
            std::mem::size_of::<CardCommand>(),
        )
    };
    stream
        .read_exact(bytes)
        .map_err(|_| Px4CardError::Unavailable)?;
    if command.card_present > 1 || command.card_initialized > 1 {
        return Err(Px4CardError::InvalidResponse);
    }
    if command.status != STATUS_SUCCEEDED {
        return Err(if command.error == ERROR_NO_CARD {
            Px4CardError::NoCard
        } else {
            Px4CardError::Unavailable
        });
    }
    Ok(())
}

fn verify_protocol(stream: &mut UnixStream) -> Result<(), Px4CardError> {
    let mut command = CardCommand::new(CODE_GET_VERSION);
    exchange(stream, &mut command)?;
    if command.version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(Px4CardError::InvalidResponse)
    }
}

fn read_names(stream: &mut UnixStream) -> Result<Vec<String>, Px4CardError> {
    let mut command = CardCommand::new(CODE_LIST_READERS);
    exchange(stream, &mut command)?;
    let count = command.reader_count as usize;
    if count > MAX_READERS {
        return Err(Px4CardError::InvalidResponse);
    }
    let mut readers = Vec::with_capacity(count);
    for field in &command.readers[..count] {
        let name = CardCommand::reader_name(field).ok_or(Px4CardError::InvalidResponse)?;
        if !name.is_empty() {
            readers.push(name);
        }
    }
    Ok(readers)
}

/// Enumerate DriverHost reader names using a fresh short-lived connection.
fn query_readers() -> Result<Vec<String>, Px4CardError> {
    let mut stream = connect_socket_with_timeout(READER_PROBE_TIMEOUT)?;
    verify_protocol(&mut stream)?;
    read_names(&mut stream)
}

/// Enumerate DriverHost readers as virtual PC/SC names. A short-lived socket
/// connection is enough for enumeration; card ownership starts at SCardConnect.
/// Cache results briefly because PC/SC calls this once for sizing and again to
/// fill its output buffer.
pub(crate) fn list_readers() -> Vec<String> {
    let cache = READER_CACHE.get_or_init(|| Mutex::new(ReaderCache::default()));
    readers_with_cache(cache, query_readers)
        .into_iter()
        .map(|name| format!("{READER_PREFIX}{name}"))
        .collect()
}

fn allocate_handle() -> Option<usize> {
    for _ in 0..1024 {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        if handle >= 0x6000_0000 {
            NEXT_HANDLE.store(0x5000_0000, Ordering::Relaxed);
            continue;
        }
        if sessions().contains_key(&handle) {
            continue;
        }
        if boncaslink::reserve_px4_card(handle) {
            return Some(handle);
        }
    }
    None
}

pub(crate) fn connect_reader(context: usize, name: &str) -> Result<(usize, u32), Px4CardError> {
    let reader = name
        .strip_prefix(READER_PREFIX)
        .ok_or(Px4CardError::InvalidReaderName)?;
    let _context_operation = begin_context_operation(context)?;
    connect_reader_with_stream(context, reader, connect_socket()?)
}

fn connect_reader_with_stream(
    context: usize,
    reader: &str,
    mut stream: UnixStream,
) -> Result<(usize, u32), Px4CardError> {
    verify_protocol(&mut stream)?;
    if !read_names(&mut stream)?
        .iter()
        .any(|candidate| candidate == reader)
    {
        return Err(Px4CardError::InvalidReaderName);
    }

    let mut command = CardCommand::new(CODE_CONNECT);
    command.share_mode = SHARE_MODE_SHARED;
    command.set_reader_name(reader)?;
    exchange(&mut stream, &mut command)?;

    let mut status = CardCommand::new(CODE_STATUS);
    status.set_reader_name(reader)?;
    if let Err(error) = exchange(&mut stream, &mut status) {
        let mut disconnect = CardCommand::new(CODE_DISCONNECT);
        let _ = exchange(&mut stream, &mut disconnect);
        return Err(error);
    }
    if let Some(error) = card_status_error(&status) {
        let mut disconnect = CardCommand::new(CODE_DISCONNECT);
        let _ = exchange(&mut stream, &mut disconnect);
        return Err(error);
    }

    let shared = match shared_session(stream) {
        Ok(shared) => shared,
        Err((error, mut stream)) => {
            let mut disconnect = CardCommand::new(CODE_DISCONNECT);
            let _ = exchange(&mut stream, &mut disconnect);
            return Err(error);
        }
    };
    let Some(handle) = allocate_handle() else {
        let mut session = lock_session(&shared);
        let mut disconnect = CardCommand::new(CODE_DISCONNECT);
        let _ = exchange(&mut session.stream, &mut disconnect);
        return Err(Px4CardError::Unavailable);
    };
    if !register_session_for_context(context, handle, Arc::clone(&shared)) {
        boncaslink::release_px4_card(handle);
        let mut session = lock_session(&shared);
        let mut disconnect = CardCommand::new(CODE_DISCONNECT);
        let _ = exchange(&mut session.stream, &mut disconnect);
        return Err(Px4CardError::Unavailable);
    }
    // DriverHost's internal card path speaks T=1, matching px4rec.
    Ok((handle, 2))
}

pub(crate) fn is_card(handle: usize) -> bool {
    sessions().contains_key(&handle)
}

fn remove_session_if_current(handle: usize, shared: &SharedCardSession) -> bool {
    let removed = {
        let mut sessions = sessions();
        if sessions
            .get(&handle)
            .is_some_and(|(_, existing)| Arc::ptr_eq(existing, shared))
        {
            sessions.remove(&handle);
            true
        } else {
            false
        }
    };
    if removed {
        boncaslink::release_px4_card(handle);
    }
    removed
}

pub(crate) fn disconnect(handle: usize) -> Result<(), Px4CardError> {
    let (shared, _context_operation) = session_operation(handle)?;
    let result = {
        let mut session = lock_session(&shared);
        if session.closed {
            return Err(Px4CardError::Unavailable);
        }
        // Keep the handle recognizable by the PC/SC shim until any in-flight
        // transmit has observed this state and released its per-card lock.
        session.closed = true;
        let result = if session.poisoned {
            Err(Px4CardError::Unavailable)
        } else {
            let mut command = CardCommand::new(CODE_DISCONNECT);
            exchange(&mut session.stream, &mut command)
        };
        if result.is_err() {
            session.poisoned = true;
            let _ = session.stream.shutdown(Shutdown::Both);
        }
        result
    };
    // A failed disconnect leaves DriverHost without a reliable card state.
    // Close its socket now and retire this handle; waiting for context release
    // can otherwise keep the DriverHost card session alive indefinitely.
    remove_session_if_current(handle, &shared);
    result
}

/// Disconnect all PX4 card sessions created through this PC/SC context.
///
/// A native-context release first marks this registry entry retiring and drains
/// active operations. The backend's release result then decides whether these
/// sessions are closed or the context is restored.
pub(crate) fn begin_context_release(context: usize) -> ContextRelease {
    begin_context_release_with(context, || {})
}

fn begin_context_release_with(context: usize, mut on_waiting: impl FnMut()) -> ContextRelease {
    let release = retire_context(context);
    if release.registered {
        wait_for_context_operations(context, &mut on_waiting);
    }
    release
}

fn retire_context(context: usize) -> ContextRelease {
    let mut contexts = live_contexts();
    let Some(state) = contexts.get_mut(&context) else {
        return ContextRelease {
            context,
            registered: false,
            finished: true,
        };
    };
    if state.retiring {
        return ContextRelease {
            context,
            registered: false,
            finished: true,
        };
    }
    state.retiring = true;
    ContextRelease {
        context,
        registered: true,
        finished: false,
    }
}

fn wait_for_context_operations(context: usize, on_waiting: &mut impl FnMut()) {
    let mut contexts = live_contexts();
    while contexts
        .get(&context)
        .is_some_and(|state| state.active_operations != 0)
    {
        on_waiting();
        contexts = CONTEXT_OPERATIONS_CHANGED
            .get_or_init(Condvar::new)
            .wait(contexts)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

pub(crate) fn finish_context_release(mut release: ContextRelease, succeeded: bool) {
    if release.registered {
        let owned_sessions = {
            let mut contexts = live_contexts();
            if !contexts
                .get(&release.context)
                .is_some_and(|state| state.retiring)
            {
                Vec::new()
            } else if !succeeded {
                if let Some(state) = contexts.get_mut(&release.context) {
                    state.retiring = false;
                }
                Vec::new()
            } else {
                let mut sessions = sessions();
                let handles = sessions
                    .iter()
                    .filter_map(|(handle, (context, _))| {
                        (*context == release.context).then_some(*handle)
                    })
                    .collect::<Vec<_>>();
                let owned = handles
                    .into_iter()
                    .filter_map(|handle| {
                        sessions.remove(&handle).map(|(_, shared)| (handle, shared))
                    })
                    .collect::<Vec<_>>();
                contexts.remove(&release.context);
                owned
            }
        };

        // Session I/O locks and shutdown are taken only after registry locks
        // are released. Operations were drained before the backend release.
        for (handle, shared) in owned_sessions {
            let _ = shared.shutdown_stream.shutdown(Shutdown::Both);
            {
                let mut session = lock_session(&shared);
                session.closed = true;
                session.poisoned = true;
            }
            boncaslink::release_px4_card(handle);
        }
    }
    CONTEXT_OPERATIONS_CHANGED
        .get_or_init(Condvar::new)
        .notify_all();
    release.finished = true;
}

impl Drop for ContextRelease {
    fn drop(&mut self) {
        if !self.finished && self.registered {
            if let Some(state) = live_contexts().get_mut(&self.context) {
                state.retiring = false;
            }
            CONTEXT_OPERATIONS_CHANGED
                .get_or_init(Condvar::new)
                .notify_all();
            self.finished = true;
        }
    }
}

/// Dummy contexts have no fallible backend release, so their PX4 sessions are
/// retired immediately through the same accounting path.
pub(crate) fn release_context(context: usize) {
    // Dummy contexts have no backend release that can fail. Shutdown socket
    // clones first so blocked I/O drains, then finish teardown transactionally.
    let release = retire_context(context);
    if !release.registered {
        return;
    }
    let owned_sessions = {
        let contexts = live_contexts();
        if !contexts.get(&context).is_some_and(|state| state.retiring) {
            Vec::new()
        } else {
            sessions()
                .iter()
                .filter_map(|(handle, (owner, shared))| {
                    (*owner == context).then(|| (*handle, Arc::clone(shared)))
                })
                .collect::<Vec<_>>()
        }
    };
    for (_, shared) in &owned_sessions {
        let _ = shared.shutdown_stream.shutdown(Shutdown::Both);
    }
    wait_for_context_operations(context, &mut || {});
    finish_context_release(release, true);
}

pub(crate) fn transmit_with_capacity(
    handle: usize,
    apdu: &[u8],
    response_capacity: usize,
) -> Result<Vec<u8>, Px4TransmitError> {
    if apdu.is_empty() || apdu.len() > MAX_DATA_SIZE {
        return Err(Px4TransmitError::InvalidParameter);
    }
    let (shared, _context_operation) = session_operation(handle).map_err(Px4TransmitError::Card)?;
    let mut session = lock_session(&shared);
    if session.closed || session.poisoned {
        return Err(Px4TransmitError::Card(Px4CardError::Unavailable));
    }
    if let Some(pending) = session.pending_response.as_ref() {
        if pending.apdu != apdu {
            // A different command means the caller abandoned the buffer retry.
            // Drop the old response and allow the new APDU to be transmitted.
            session.pending_response = None;
        } else {
            if response_capacity < pending.response.len() {
                return Err(Px4TransmitError::InsufficientBuffer(pending.response.len()));
            }
            return session
                .pending_response
                .take()
                .map(|pending| pending.response)
                .ok_or(Px4TransmitError::Card(Px4CardError::Unavailable));
        }
    }
    let mut command = CardCommand::new(CODE_TRANSMIT);
    command.data_length = apdu.len() as u32;
    command.data_capacity = MAX_DATA_SIZE as u32;
    command.data[..apdu.len()].copy_from_slice(apdu);
    if let Err(error) = exchange(&mut session.stream, &mut command) {
        if error != Px4CardError::NoCard {
            session.poisoned = true;
        }
        return Err(Px4TransmitError::Card(error));
    }
    let length = command.data_length as usize;
    if length > MAX_DATA_SIZE {
        session.poisoned = true;
        return Err(Px4TransmitError::Card(Px4CardError::InvalidResponse));
    }
    let response = command.data[..length].to_vec();
    if response_capacity < response.len() {
        let required_length = response.len();
        session.pending_response = Some(PendingResponse {
            apdu: apdu.to_vec(),
            response,
        });
        return Err(Px4TransmitError::InsufficientBuffer(required_length));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boncaslink;
    use std::sync::mpsc;

    static HANDLE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn command_bytes(command: &CardCommand) -> Vec<u8> {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (command as *const CardCommand).cast::<u8>(),
                std::mem::size_of::<CardCommand>(),
            )
        };
        bytes.to_vec()
    }

    fn successful_response(code: u32) -> Vec<u8> {
        let mut response = CardCommand::new(code);
        response.status = STATUS_SUCCEEDED;
        command_bytes(&response)
    }

    fn successful_transmit_response() -> Vec<u8> {
        let mut response = CardCommand::new(CODE_TRANSMIT);
        response.status = STATUS_SUCCEEDED;
        response.data[..2].copy_from_slice(&[0x90, 0x00]);
        response.data_length = 2;
        command_bytes(&response)
    }

    fn register_test_session(context: usize, stream: UnixStream) -> usize {
        let shared = shared_session(stream).unwrap();
        register_context(context);
        let handle = allocate_handle().unwrap();
        assert!(register_session_for_context(context, handle, shared));
        handle
    }

    #[test]
    fn command_layout_matches_px4_drv_protocol_v2() {
        assert_eq!(std::mem::size_of::<CardCommand>(), 9304);
        assert_eq!(std::mem::align_of::<CardCommand>(), 8);
    }

    #[test]
    fn reader_probe_is_cached_for_pcsc_size_and_fill_calls() {
        let cache = Mutex::new(ReaderCache::default());
        let probes = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            probes.fetch_add(1, Ordering::Relaxed);
            Ok(vec!["PX4 DriverHost: Reader".to_owned()])
        };

        assert_eq!(
            readers_with_cache(&cache, probe),
            ["PX4 DriverHost: Reader"]
        );
        assert_eq!(
            readers_with_cache(&cache, || {
                probes.fetch_add(1, Ordering::Relaxed);
                Ok(vec!["PX4 DriverHost: Reader".to_owned()])
            }),
            ["PX4 DriverHost: Reader"]
        );
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reader_probe_has_a_short_socket_timeout() {
        assert!(READER_PROBE_TIMEOUT <= Duration::from_millis(500));
    }

    #[test]
    fn socket_peer_must_be_the_current_user_or_root() {
        assert!(peer_uid_is_trusted(0, 501));
        assert!(peer_uid_is_trusted(501, 501));
        assert!(!peer_uid_is_trusted(502, 501));
    }

    #[test]
    fn reader_probe_does_not_hold_the_cache_lock_during_io() {
        let cache = Arc::new(Mutex::new(ReaderCache::default()));
        let worker_cache = Arc::clone(&cache);
        let (started_tx, started_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            readers_with_cache(&worker_cache, || {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                Ok(vec!["PX4 DriverHost: Reader".to_owned()])
            })
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(cache.try_lock().is_ok());
        assert!(readers_with_cache(&cache, || panic!("cache hit must not probe")).is_empty());
        finish_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), ["PX4 DriverHost: Reader"]);
    }

    #[test]
    fn status_requires_an_initialized_card_before_issuing_t1_handle() {
        let mut status = CardCommand::new(CODE_STATUS);
        status.card_present = 1;
        status.card_initialized = 0;

        assert_eq!(card_status_error(&status), Some(Px4CardError::Unavailable));
    }

    #[test]
    fn clone_failure_after_connect_sends_explicit_disconnect() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0202;
        let reader = "Synthetic Reader";
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let server = std::thread::spawn(move || {
            let mut received_codes = Vec::new();
            let mut disconnect_seen = false;
            for expected_code in [
                CODE_GET_VERSION,
                CODE_LIST_READERS,
                CODE_CONNECT,
                CODE_STATUS,
                CODE_DISCONNECT,
            ] {
                let mut request_bytes = vec![0; std::mem::size_of::<CardCommand>()];
                if peer.read_exact(&mut request_bytes).is_err() {
                    break;
                }
                let request = unsafe {
                    std::ptr::read_unaligned(request_bytes.as_ptr().cast::<CardCommand>())
                };
                received_codes.push(request.code);
                if request.code != expected_code {
                    break;
                }

                let mut response = CardCommand::new(request.code);
                response.status = STATUS_SUCCEEDED;
                match request.code {
                    CODE_GET_VERSION => response.version = PROTOCOL_VERSION,
                    CODE_LIST_READERS => {
                        response.reader_count = 1;
                        let chars = reader.chars().map(|ch| ch as u32).collect::<Vec<_>>();
                        response.readers[0][..chars.len()].copy_from_slice(&chars);
                    }
                    CODE_STATUS => {
                        response.card_present = 1;
                        response.card_initialized = 1;
                    }
                    CODE_DISCONNECT => disconnect_seen = true,
                    _ => {}
                }
                peer.write_all(&command_bytes(&response)).unwrap();
            }
            let mut trailing = [0; 1];
            let peer_closed = peer.read(&mut trailing).is_ok_and(|length| length == 0);
            (received_codes, disconnect_seen, peer_closed)
        });

        FAIL_NEXT_SHUTDOWN_STREAM_CLONE.with(|fail| fail.set(true));
        let connect_result = connect_reader_with_stream(context, reader, client);
        let (received_codes, disconnect_seen, peer_closed) = server.join().unwrap();

        assert_eq!(connect_result, Err(Px4CardError::Unavailable));
        assert_eq!(
            received_codes,
            [
                CODE_GET_VERSION,
                CODE_LIST_READERS,
                CODE_CONNECT,
                CODE_STATUS,
                CODE_DISCONNECT
            ]
        );
        assert!(disconnect_seen);
        assert!(peer_closed);
    }

    #[test]
    fn ffi_transmit_different_apdu_discards_pending_response() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0201;
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let handle = register_test_session(context, client);
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                let mut request_bytes = vec![0; std::mem::size_of::<CardCommand>()];
                if peer.read_exact(&mut request_bytes).is_err() {
                    break;
                }
                let request = unsafe {
                    std::ptr::read_unaligned(request_bytes.as_ptr().cast::<CardCommand>())
                };
                requests.push(request.data[..request.data_length as usize].to_vec());
                peer.write_all(&successful_transmit_response()).unwrap();
            }
            requests
        });

        let (first_status, required_length, different_status, retry_status, response, response_len) = unsafe {
            let send_pci = 0u8;
            let first_apdu = [0x90, 0x40];
            let different_apdu = [0x90, 0x41];
            let mut required_length = 0 as _;
            let first_status = crate::pcsc_shim::SCardTransmit(
                handle as _,
                &send_pci as *const _ as *const std::ffi::c_void,
                first_apdu.as_ptr(),
                first_apdu.len() as _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut required_length,
            );
            let mut response = [0u8; 8];
            let mut response_len = response.len() as _;
            let different_status = crate::pcsc_shim::SCardTransmit(
                handle as _,
                &send_pci as *const _ as *const std::ffi::c_void,
                different_apdu.as_ptr(),
                different_apdu.len() as _,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_len,
            );
            response_len = response.len() as _;
            let retry_status = crate::pcsc_shim::SCardTransmit(
                handle as _,
                &send_pci as *const _ as *const std::ffi::c_void,
                first_apdu.as_ptr(),
                first_apdu.len() as _,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_len,
            );
            release_context(context);
            (
                first_status,
                required_length,
                different_status,
                retry_status,
                response,
                response_len,
            )
        };
        let requests = server.join().unwrap();

        assert_eq!(first_status, 0x8010_0008u32 as i32 as _);
        assert_eq!(required_length, 2);
        assert_eq!(different_status, 0);
        assert_eq!(retry_status, 0);
        assert_eq!(&response[..response_len as usize], [0x90, 0x00]);
        assert_eq!(requests, [[0x90, 0x40], [0x90, 0x41], [0x90, 0x40]]);
        assert!(!is_card(handle));
    }

    #[test]
    fn ffi_same_apdu_retry_reuses_cached_px4_response() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0206;
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let handle = register_test_session(context, client);
        let server = std::thread::spawn(move || {
            let mut request_bytes = vec![0; std::mem::size_of::<CardCommand>()];
            peer.read_exact(&mut request_bytes).unwrap();
            let request =
                unsafe { std::ptr::read_unaligned(request_bytes.as_ptr().cast::<CardCommand>()) };
            peer.write_all(&successful_transmit_response()).unwrap();
            request.data[..request.data_length as usize].to_vec()
        });

        let (first_status, required_length, retry_status, response, response_length) = unsafe {
            let send_pci = 0u8;
            let apdu = [0x90, 0x43];
            let mut small_response = [0u8; 1];
            let mut required_length = small_response.len() as _;
            let first_status = crate::pcsc_shim::SCardTransmit(
                handle as _,
                &send_pci as *const _ as *const std::ffi::c_void,
                apdu.as_ptr(),
                apdu.len() as _,
                std::ptr::null_mut(),
                small_response.as_mut_ptr(),
                &mut required_length,
            );
            let mut response = [0u8; 8];
            let mut response_length = response.len() as _;
            let retry_status = crate::pcsc_shim::SCardTransmit(
                handle as _,
                &send_pci as *const _ as *const std::ffi::c_void,
                apdu.as_ptr(),
                apdu.len() as _,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_length,
            );
            release_context(context);
            (
                first_status,
                required_length,
                retry_status,
                response,
                response_length,
            )
        };
        let request = server.join().unwrap();

        assert_eq!(first_status, 0x8010_0008u32 as i32 as _);
        assert_eq!(required_length, 2);
        assert_eq!(retry_status, 0);
        assert_eq!(&response[..response_length as usize], [0x90, 0x00]);
        assert_eq!(request, [0x90, 0x43]);
        assert!(!is_card(handle));
    }

    #[test]
    fn px4_invalid_apdu_lengths_are_invalid_parameters() {
        assert_eq!(
            transmit_with_capacity(0, &[], 0),
            Err(Px4TransmitError::InvalidParameter)
        );
        assert_eq!(
            transmit_with_capacity(0, &[0; MAX_DATA_SIZE + 1], 0),
            Err(Px4TransmitError::InvalidParameter)
        );
    }

    #[test]
    fn px4_session_registration_is_rejected_after_context_release() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0204;
        register_context(context);
        release_context(context);

        let (client, _peer) = UnixStream::pair().unwrap();
        let shared = shared_session(client).unwrap();
        assert!(!register_session_for_context(context, 0x5000_0204, shared));
        assert!(!is_card(0x5000_0204));
    }

    #[test]
    fn context_release_waits_for_active_synthetic_card_operations() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0206;
        register_context(context);
        let operation = begin_context_operation(context).unwrap();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_thread = std::thread::spawn(move || {
            let release = begin_context_release_with(context, || {
                let _ = waiting_tx.send(());
            });
            release_tx.send(release.registered).unwrap();
            finish_context_release(release, false);
        });

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(begin_context_operation(context).is_err());
        assert!(release_rx.try_recv().is_err());
        drop(operation);
        assert!(release_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        release_thread.join().unwrap();

        assert!(begin_context_operation(context).is_ok());
        release_context(context);
    }

    #[test]
    fn px4_connect_registration_racing_context_release_leaves_no_late_session() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x7100_0205;
        register_context(context);
        let handle = allocate_handle().unwrap();
        let (client, _peer) = UnixStream::pair().unwrap();
        let shared = shared_session(client).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let register_barrier = Arc::clone(&barrier);
        let register_thread = std::thread::spawn(move || {
            register_barrier.wait();
            let registered = register_session_for_context(context, handle, shared);
            if !registered {
                boncaslink::release_px4_card(handle);
            }
            registered
        });
        let release_barrier = Arc::clone(&barrier);
        let release_thread = std::thread::spawn(move || {
            release_barrier.wait();
            release_context(context);
        });
        barrier.wait();

        let registered = register_thread.join().unwrap();
        release_thread.join().unwrap();
        assert!(!is_card(handle));
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        assert!(reservation_released);
        let _ = registered;
    }

    #[test]
    fn releasing_context_shuts_down_without_waiting_for_disconnect_response() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = boncaslink::new_dummy_context();
        let (client, mut peer) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let handle = register_test_session(context, client);
        let (peer_closed_tx, peer_closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut first_byte = [0; 1];
            let peer_closed = peer.read(&mut first_byte).is_ok_and(|length| length == 0);
            peer_closed_tx.send(peer_closed).unwrap();
        });

        let status = unsafe { crate::pcsc_shim::SCardReleaseContext(context as i32) };
        let socket_closed = peer_closed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        let session_remains = is_card(handle);
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        sessions().remove(&handle);
        boncaslink::release_px4_card(handle);
        boncaslink::release_dummy_context(context);

        assert_eq!(status, 0);
        assert!(socket_closed);
        assert!(!session_remains);
        assert!(reservation_released);
    }

    #[test]
    fn context_release_does_not_disconnect_a_successfully_disconnected_card_twice() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = boncaslink::new_dummy_context();
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let handle = register_test_session(context, client);
        let (second_request_tx, second_request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            peer.read_exact(&mut request).unwrap();
            assert_eq!(
                u32::from_ne_bytes(request[..4].try_into().unwrap()),
                CODE_DISCONNECT
            );
            peer.write_all(&successful_response(CODE_DISCONNECT))
                .unwrap();
            let mut second_request = [0; 1];
            second_request_tx
                .send(peer.read(&mut second_request).is_ok_and(|len| len > 0))
                .unwrap();
        });

        assert_eq!(disconnect(handle), Ok(()));
        let session_removed_after_disconnect = !is_card(handle);
        let status = unsafe { crate::pcsc_shim::SCardReleaseContext(context as i32) };
        let second_disconnect_was_sent = second_request_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        server.join().unwrap();
        boncaslink::release_dummy_context(context);

        assert_eq!(status, 0);
        assert!(session_removed_after_disconnect);
        assert!(!second_disconnect_was_sent);
    }

    #[test]
    fn disconnect_failure_shuts_down_and_releases_the_session() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = boncaslink::new_dummy_context();
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let handle = register_test_session(context, client);
        let (request_count_tx, request_count_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            let first_seen = peer.read_exact(&mut request).is_ok();
            request_count_tx.send(first_seen).unwrap();
            if first_seen {
                let mut failed = CardCommand::new(CODE_DISCONNECT);
                failed.error = 1;
                peer.write_all(&command_bytes(&failed)).unwrap();
            }
            let mut second_request = [0; 1];
            let socket_closed = peer.read(&mut second_request).is_ok_and(|len| len == 0);
            request_count_tx.send(socket_closed).unwrap();
        });

        assert_eq!(disconnect(handle), Err(Px4CardError::Unavailable));
        let retained_after_disconnect_failure = is_card(handle);
        let first_disconnect_was_sent = request_count_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let socket_closed_after_disconnect_failure = request_count_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        let status = unsafe { crate::pcsc_shim::SCardReleaseContext(context as i32) };
        server.join().unwrap();
        let session_remains = is_card(handle);
        sessions().remove(&handle);
        boncaslink::release_px4_card(handle);
        boncaslink::release_dummy_context(context);

        assert!(!retained_after_disconnect_failure);
        assert_eq!(status, 0);
        assert!(first_disconnect_was_sent);
        assert!(socket_closed_after_disconnect_failure);
        assert!(!session_remains);
        assert!(reservation_released);
    }

    #[test]
    fn failed_disconnect_racing_context_release_retires_one_handle() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = boncaslink::new_dummy_context();
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let handle = register_test_session(context, client);
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let disconnect_barrier = Arc::clone(&barrier);
        let disconnect_thread = std::thread::spawn(move || {
            disconnect_barrier.wait();
            disconnect(handle)
        });
        let release_barrier = Arc::clone(&barrier);
        let release_thread = std::thread::spawn(move || {
            release_barrier.wait();
            release_context(context);
        });

        barrier.wait();
        let mut request = vec![0; std::mem::size_of::<CardCommand>()];
        let mut request_length = 0;
        while request_length < request.len() {
            match peer.read(&mut request[request_length..]) {
                Ok(0) => break,
                Ok(length) => request_length += length,
                Err(_) => break,
            }
        }
        let complete_disconnect_request = request_length == request.len()
            && u32::from_ne_bytes(request[..4].try_into().unwrap()) == CODE_DISCONNECT;
        if complete_disconnect_request {
            let mut failed = CardCommand::new(CODE_DISCONNECT);
            failed.error = 1;
            let _ = peer.write_all(&command_bytes(&failed));
        }

        let disconnect_result = disconnect_thread.join().unwrap();
        release_thread.join().unwrap();
        let mut trailing = [0; 1];
        let mut duplicate_request_bytes = 0;
        let peer_closed = loop {
            match peer.read(&mut trailing) {
                Ok(0) => break true,
                Ok(length) => duplicate_request_bytes += length,
                Err(_) => break false,
            }
        };
        let session_remains = is_card(handle);
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        boncaslink::release_dummy_context(context);

        assert_eq!(disconnect_result, Err(Px4CardError::Unavailable));
        assert!(peer_closed);
        assert_eq!(duplicate_request_bytes, 0);
        assert!(!session_remains);
        assert!(reservation_released);
    }

    #[test]
    fn context_release_interrupts_a_blocked_transmit() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = boncaslink::new_dummy_context();
        let (client, mut peer) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let handle = register_test_session(context, client);
        let (request_tx, request_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            peer.read_exact(&mut request).unwrap();
            request_tx
                .send(u32::from_ne_bytes(request[..4].try_into().unwrap()))
                .unwrap();
            if finish_rx.recv().unwrap() {
                peer.write_all(&successful_response(CODE_TRANSMIT)).unwrap();
                let mut disconnect = vec![0; std::mem::size_of::<CardCommand>()];
                peer.read_exact(&mut disconnect).unwrap();
                assert_eq!(
                    u32::from_ne_bytes(disconnect[..4].try_into().unwrap()),
                    CODE_DISCONNECT
                );
                peer.write_all(&successful_response(CODE_DISCONNECT))
                    .unwrap();
            }
        });

        let transmit_thread =
            std::thread::spawn(move || transmit_with_capacity(handle, &[0x90], usize::MAX));
        assert_eq!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            CODE_TRANSMIT
        );
        let (release_tx, release_rx) = mpsc::channel();
        let release_thread = std::thread::spawn(move || {
            release_context(context);
            release_tx.send(()).unwrap();
        });

        let release_finished_promptly = release_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        finish_tx.send(!release_finished_promptly).unwrap();
        if !release_finished_promptly {
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let transmit_result = transmit_thread.join().unwrap();
        release_thread.join().unwrap();
        server.join().unwrap();
        let session_remains = is_card(handle);
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        boncaslink::release_dummy_context(context);

        assert!(release_finished_promptly);
        assert_eq!(
            transmit_result,
            Err(Px4TransmitError::Card(Px4CardError::Unavailable))
        );
        assert!(!session_remains);
        assert!(reservation_released);
    }

    #[test]
    fn native_context_release_error_preserves_px4_sessions() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x6000_0101;
        let native_card = 0x5000_0010;
        boncaslink::register_native_context(context);
        let native_alias = boncaslink::register_native_card_alias(context, native_card).unwrap();
        assert_ne!(native_alias, native_card);

        let (client, mut peer) = UnixStream::pair().unwrap();
        let handle = register_test_session(context, client);
        let server = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            peer.read_exact(&mut request).unwrap();
            peer.write_all(&successful_transmit_response()).unwrap();
        });

        let session_was_available_during_native_release = std::sync::atomic::AtomicBool::new(false);
        let synthetic_operations_were_blocked = std::sync::atomic::AtomicBool::new(false);
        let status = crate::pcsc_shim::release_native_context_with(context as _, |_| {
            session_was_available_during_native_release.store(is_card(handle), Ordering::Relaxed);
            let connect_blocked = begin_context_operation(context).is_err();
            let transmit_blocked = transmit_with_capacity(handle, &[0x90], usize::MAX)
                == Err(Px4TransmitError::Card(Px4CardError::Unavailable));
            let disconnect_blocked = disconnect(handle) == Err(Px4CardError::Unavailable);
            synthetic_operations_were_blocked.store(
                connect_blocked && transmit_blocked && disconnect_blocked,
                Ordering::Relaxed,
            );
            -1
        });
        let session_remains = is_card(handle);
        let original_session_still_works =
            transmit_with_capacity(handle, &[0x90], usize::MAX) == Ok(vec![0x90, 0]);
        let (reconnect_client, _reconnect_peer) = UnixStream::pair().unwrap();
        let reconnect_handle = allocate_handle().unwrap();
        let reconnect_succeeded = register_session_for_context(
            context,
            reconnect_handle,
            shared_session(reconnect_client).unwrap(),
        );
        release_context(context);
        let reconnect_session_remains = is_card(reconnect_handle);
        server.join().unwrap();
        let reservation_released = boncaslink::reserve_px4_card(handle);
        if reservation_released {
            boncaslink::release_px4_card(handle);
        }
        let raw_native_handle_can_be_used_in_the_px4_range =
            boncaslink::reserve_px4_card(native_card);
        if raw_native_handle_can_be_used_in_the_px4_range {
            boncaslink::release_px4_card(native_card);
        }
        let alias_restored_after_release_failure =
            boncaslink::resolve_native_card(native_alias) == Some(native_card);

        boncaslink::unregister_native_context(context);

        assert_eq!(status, -1);
        assert!(session_was_available_during_native_release.load(Ordering::Relaxed));
        assert!(synthetic_operations_were_blocked.load(Ordering::Relaxed));
        assert!(session_remains);
        assert!(original_session_still_works);
        assert!(reconnect_succeeded);
        assert!(!reconnect_session_remains);
        assert!(reservation_released);
        assert!(raw_native_handle_can_be_used_in_the_px4_range);
        assert!(alias_restored_after_release_failure);
    }

    #[test]
    fn successful_native_release_closes_px4_sessions_after_backend_success() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let context = 0x6000_0102;
        assert!(boncaslink::register_native_context(context));
        register_context(context);
        let (client, _peer) = UnixStream::pair().unwrap();
        let handle = register_test_session(context, client);

        let mut session_was_available_during_native_release = false;
        let status = crate::pcsc_shim::release_native_context_with(context as _, |_| {
            session_was_available_during_native_release = is_card(handle);
            0
        });

        assert_eq!(status, 0);
        assert!(session_was_available_during_native_release);
        assert!(!is_card(handle));
        assert!(begin_context_operation(context).is_err());
        assert!(boncaslink::reserve_px4_card(handle));
        boncaslink::release_px4_card(handle);
    }

    #[test]
    fn command_reader_name_uses_macos_wchar_width() {
        let mut command = CardCommand::new(CODE_CONNECT);
        command.set_reader_name("PIXELA Smart Card Reader").unwrap();
        assert_eq!(
            CardCommand::reader_name(&command.reader_name).as_deref(),
            Some("PIXELA Smart Card Reader")
        );
    }

    #[test]
    fn exchange_rejects_non_boolean_card_status_bytes() {
        for offset in [
            std::mem::offset_of!(CardCommand, card_present),
            std::mem::offset_of!(CardCommand, card_initialized),
        ] {
            let (mut client, mut peer) = UnixStream::pair().unwrap();
            let server = std::thread::spawn(move || {
                let mut request = vec![0; std::mem::size_of::<CardCommand>()];
                peer.read_exact(&mut request).unwrap();
                let mut response = successful_response(CODE_STATUS);
                response[offset] = 2;
                peer.write_all(&response).unwrap();
            });

            let mut command = CardCommand::new(CODE_STATUS);
            assert_eq!(
                exchange(&mut client, &mut command),
                Err(Px4CardError::InvalidResponse)
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn partial_response_poisons_the_card_session() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let (client, mut peer) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let handle = register_test_session(0, client);
        let (request_count_tx, request_count_rx) = mpsc::channel();

        let server = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            peer.read_exact(&mut request).unwrap();
            let response = successful_response(CODE_TRANSMIT);
            peer.write_all(&response[..32]).unwrap();

            let second_request_seen = peer.read_exact(&mut request).is_ok();
            request_count_tx.send(second_request_seen).unwrap();
        });

        assert_eq!(
            transmit_with_capacity(handle, &[0x90], usize::MAX),
            Err(Px4TransmitError::Card(Px4CardError::Unavailable))
        );
        assert_eq!(
            transmit_with_capacity(handle, &[0x90], usize::MAX),
            Err(Px4TransmitError::Card(Px4CardError::Unavailable))
        );
        assert!(!request_count_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap());
        sessions().remove(&handle);
        boncaslink::release_px4_card(handle);
        server.join().unwrap();
    }

    #[test]
    fn blocked_card_io_does_not_block_a_different_card_session() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let (client_a, mut peer_a) = UnixStream::pair().unwrap();
        let (client_b, mut peer_b) = UnixStream::pair().unwrap();
        client_a
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client_b
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let handle_a = register_test_session(0, client_a);
        let handle_b = register_test_session(0, client_b);

        let tx_a =
            std::thread::spawn(move || transmit_with_capacity(handle_a, &[0x90], usize::MAX));
        let mut request_a = vec![0; std::mem::size_of::<CardCommand>()];
        peer_a.read_exact(&mut request_a).unwrap();

        let tx_b =
            std::thread::spawn(move || transmit_with_capacity(handle_b, &[0x90], usize::MAX));
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let server_b = std::thread::spawn(move || {
            let mut request = vec![0; std::mem::size_of::<CardCommand>()];
            peer_b.read_exact(&mut request).unwrap();
            request_seen_tx.send(()).unwrap();
            let mut response = CardCommand::new(CODE_TRANSMIT);
            response.status = STATUS_SUCCEEDED;
            response.data[0] = 0x90;
            response.data_length = 1;
            peer_b.write_all(&command_bytes(&response)).unwrap();
        });
        let tx_b_observed_while_a_blocked = request_seen_rx
            .recv_timeout(Duration::from_millis(250))
            .is_ok();

        peer_a
            .write_all(&successful_response(CODE_TRANSMIT))
            .unwrap();
        assert_eq!(tx_a.join().unwrap(), Ok(vec![]));
        assert_eq!(tx_b.join().unwrap(), Ok(vec![0x90]));
        server_b.join().unwrap();
        sessions().remove(&handle_a);
        sessions().remove(&handle_b);
        boncaslink::release_px4_card(handle_a);
        boncaslink::release_px4_card(handle_b);

        assert!(
            tx_b_observed_while_a_blocked,
            "a blocked card session serialized I/O for a different card"
        );
    }

    #[test]
    fn px4_handle_allocator_can_use_raw_native_handle_range_with_aliases() {
        let _guard = HANDLE_TEST_LOCK.lock().unwrap();
        let candidate = 0x5000_6000;
        let context = 0x5000_6001;
        let previous = NEXT_HANDLE.swap(candidate, Ordering::Relaxed);
        boncaslink::register_native_context(context);
        let alias = boncaslink::register_native_card_alias(context, candidate).unwrap();
        assert_ne!(alias, candidate);

        let allocated = allocate_handle().unwrap();

        assert_eq!(allocated, candidate);
        assert_eq!(boncaslink::resolve_native_card(alias), Some(candidate));
        boncaslink::release_px4_card(allocated);
        boncaslink::unregister_native_context(context);
        NEXT_HANDLE.store(previous, Ordering::Relaxed);
    }
}
