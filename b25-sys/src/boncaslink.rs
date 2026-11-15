//! BonCasLink virtual PC/SC reader shared by the platform shims.
//! Called across a C ABI boundary: all state access recovers poisoned locks.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

pub const BONCASLINK_READER_NAME: &str = "BonCasLink Client Card Reader";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

static SERVER: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// One entry per virtual card handle. Each session has its own lock so that a
/// slow or dead server only stalls the decoder that is talking to it: the map
/// lock is also taken by `is_card`, which every local-reader call goes through.
struct PendingResponse {
    request: Vec<u8>,
    response: Vec<u8>,
}

struct CardSession {
    stream: Option<TcpStream>,
    pending_response: Option<PendingResponse>,
}

type Card = Arc<Mutex<CardSession>>;
struct CardEntry {
    card: Card,
    address: String,
    owner_context: Option<usize>,
}
static CARDS: OnceLock<Mutex<HashMap<usize, CardEntry>>> = OnceLock::new();
static REGISTERED_READERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
#[cfg(target_os = "macos")]
static PX4_CARDS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
static DUMMY_CONTEXTS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
const NATIVE_ALIAS_BASE: usize = 0x2000_0000;
const NATIVE_ALIAS_CAPACITY: usize = 0x2000_0000;
const NATIVE_ALIAS_MASK: usize = NATIVE_ALIAS_CAPACITY - 1;
static NEXT_NATIVE_ALIAS: AtomicUsize = AtomicUsize::new(1);

#[derive(Default)]
struct NativeContextRegistration {
    aliases: HashSet<usize>,
    retiring: bool,
    active_operations: usize,
}

#[derive(Clone, Copy)]
struct NativeCardAlias {
    context: usize,
    backend_handle: usize,
    active: bool,
    disconnecting: bool,
    in_flight: usize,
}

/// These are shim-owned card handles returned to callers. The backend's native
/// SCARDHANDLE remains private in each entry and is never range-filtered.
static NATIVE_CARD_ALIASES: OnceLock<Mutex<HashMap<usize, NativeCardAlias>>> = OnceLock::new();
static NATIVE_CONTEXTS: OnceLock<Mutex<HashMap<usize, NativeContextRegistration>>> =
    OnceLock::new();
static NATIVE_CONTEXT_OPERATIONS_CHANGED: OnceLock<Condvar> = OnceLock::new();
static NATIVE_CARD_OPERATIONS_CHANGED: OnceLock<Condvar> = OnceLock::new();
/// Serialize context establish-registration against release-retire-backend
/// release. This prevents a reused numeric context id from being removed by
/// cleanup belonging to the context that was released just before it.
static NATIVE_CONTEXT_LIFECYCLE: OnceLock<Mutex<()>> = OnceLock::new();
static NEXT_CARD: AtomicUsize = AtomicUsize::new(1);
static NEXT_CONTEXT: AtomicUsize = AtomicUsize::new(0x1000);

pub(crate) struct NativeContextRelease {
    context: usize,
    aliases: Vec<usize>,
    registered: bool,
    #[cfg(test)]
    waited_for_operations: bool,
    finished: bool,
    lifecycle: Option<MutexGuard<'static, ()>>,
}

pub(crate) struct NativeContextOperation {
    context: usize,
}

pub(crate) struct NativeCardOperation {
    alias: usize,
    context: usize,
    backend_handle: usize,
}

pub(crate) struct NativeCardDisconnect {
    alias: usize,
    context: usize,
    backend_handle: usize,
    finished: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardTransmitError {
    Unavailable,
    InsufficientBuffer(usize),
}

fn lock<T: Default>(cell: &'static OnceLock<Mutex<T>>) -> MutexGuard<'static, T> {
    cell.get_or_init(|| Mutex::new(T::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn set_server(addr: Option<&str>) -> Result<(), String> {
    let normalized = match addr.map(str::trim) {
        None | Some("") => None,
        Some(value) => match normalize_address(value) {
            Ok(value) => Some(value),
            Err(error) => {
                *lock(&SERVER) = None;
                return Err(error);
            }
        },
    };
    *lock(&SERVER) = normalized;
    Ok(())
}

pub fn enabled() -> bool {
    lock(&SERVER).is_some()
}

pub fn has_registered_readers() -> bool {
    !lock(&REGISTERED_READERS).is_empty()
}

pub fn new_dummy_context() -> usize {
    let mut contexts = lock(&DUMMY_CONTEXTS);
    for _ in 0..1024 {
        let id = NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed);
        if id != 0 && !contexts.contains(&id) && !lock(&NATIVE_CONTEXTS).contains_key(&id) {
            contexts.insert(id);
            return id;
        }
    }
    0
}

pub fn is_dummy_context(id: usize) -> bool {
    lock(&DUMMY_CONTEXTS).contains(&id)
}

pub fn release_dummy_context(id: usize) -> bool {
    let removed = lock(&DUMMY_CONTEXTS).remove(&id);
    if removed {
        release_context_cards(id);
    }
    removed
}

pub fn register_native_context(id: usize) -> bool {
    if id == 0 {
        return false;
    }
    // EstablishContext can return an id just released by another thread. Wait
    // for that release's backend call and finalization before registering it.
    let _lifecycle = NATIVE_CONTEXT_LIFECYCLE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Keep this lock order aligned with `new_dummy_context`: PC/SC native and
    // synthetic context handles share one numeric namespace.
    let dummy_contexts = lock(&DUMMY_CONTEXTS);
    if dummy_contexts.contains(&id) {
        return false;
    }
    let mut native_contexts = lock(&NATIVE_CONTEXTS);
    if native_contexts.contains_key(&id) {
        return false;
    }
    native_contexts.insert(id, NativeContextRegistration::default());
    true
}

#[cfg(test)]
pub fn unregister_native_context(id: usize) {
    let release = begin_native_context_release(id);
    if release.was_registered() {
        release_context_cards(id);
    }
    finish_native_context_release(release, true);
}

pub fn begin_native_context_release(context: usize) -> NativeContextRelease {
    begin_native_context_release_with(context, || {})
}

fn begin_native_context_release_with(
    context: usize,
    mut on_waiting: impl FnMut(),
) -> NativeContextRelease {
    let lifecycle = lock_native_context_lifecycle();
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let Some(registration) = contexts.get_mut(&context) else {
        return unregistered_context_release(context);
    };
    if registration.retiring {
        // Another release owns the transition. Returning invalid prevents two
        // native SCardReleaseContext calls from both reaching the backend.
        return unregistered_context_release(context);
    }
    registration.retiring = true;
    let aliases = registration.aliases.iter().copied().collect::<Vec<_>>();
    let mut native_aliases = lock(&NATIVE_CARD_ALIASES);
    for alias in &aliases {
        if let Some(entry) = native_aliases.get_mut(alias) {
            entry.active = false;
        }
    }
    drop(native_aliases);
    drop(lifecycle);

    #[cfg(test)]
    let mut waited_for_operations = false;
    while contexts
        .get(&context)
        .is_some_and(|registration| registration.active_operations != 0)
    {
        #[cfg(test)]
        {
            waited_for_operations = true;
        }
        on_waiting();
        contexts = NATIVE_CONTEXT_OPERATIONS_CHANGED
            .get_or_init(Condvar::new)
            .wait(contexts)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    drop(contexts);

    // The numeric id stays registered and retiring while the global mutex is
    // released. Reacquire it before the backend release to serialize id reuse.
    let lifecycle = lock_native_context_lifecycle();
    let contexts = lock(&NATIVE_CONTEXTS);
    let registered = contexts
        .get(&context)
        .is_some_and(|registration| registration.retiring);
    drop(contexts);
    if !registered {
        return unregistered_context_release(context);
    }
    NativeContextRelease {
        context,
        aliases,
        registered,
        #[cfg(test)]
        waited_for_operations,
        finished: false,
        lifecycle: Some(lifecycle),
    }
}

fn unregistered_context_release(context: usize) -> NativeContextRelease {
    NativeContextRelease {
        context,
        aliases: Vec::new(),
        registered: false,
        #[cfg(test)]
        waited_for_operations: false,
        finished: true,
        lifecycle: None,
    }
}

fn lock_native_context_lifecycle() -> MutexGuard<'static, ()> {
    NATIVE_CONTEXT_LIFECYCLE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl NativeContextRelease {
    pub(crate) fn was_registered(&self) -> bool {
        self.registered
    }

    pub(crate) fn release_lifecycle_lock(&mut self) {
        self.lifecycle.take();
    }

    pub(crate) fn reacquire_lifecycle_lock(&mut self) {
        if self.registered && self.lifecycle.is_none() {
            self.lifecycle = Some(lock_native_context_lifecycle());
        }
    }
}

impl NativeCardOperation {
    pub(crate) fn backend_handle(&self) -> usize {
        self.backend_handle
    }
}

impl NativeCardDisconnect {
    pub(crate) fn backend_handle(&self) -> usize {
        self.backend_handle
    }
}

fn release_native_context_operation(context: usize) {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    if let Some(registration) = contexts.get_mut(&context) {
        registration.active_operations = registration.active_operations.saturating_sub(1);
        if registration.active_operations == 0 {
            NATIVE_CONTEXT_OPERATIONS_CHANGED
                .get_or_init(Condvar::new)
                .notify_all();
        }
    }
}

impl Drop for NativeContextOperation {
    fn drop(&mut self) {
        release_native_context_operation(self.context);
    }
}

impl Drop for NativeCardOperation {
    fn drop(&mut self) {
        let mut contexts = lock(&NATIVE_CONTEXTS);
        let mut aliases = lock(&NATIVE_CARD_ALIASES);
        if let Some(entry) = aliases.get_mut(&self.alias) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        if let Some(registration) = contexts.get_mut(&self.context) {
            registration.active_operations = registration.active_operations.saturating_sub(1);
            if registration.active_operations == 0 {
                NATIVE_CONTEXT_OPERATIONS_CHANGED
                    .get_or_init(Condvar::new)
                    .notify_all();
            }
        }
        NATIVE_CARD_OPERATIONS_CHANGED
            .get_or_init(Condvar::new)
            .notify_all();
    }
}

/// Keep a native connect call in flight as one operation with respect to its
/// context release. Registrations already marked retiring reject new work.
pub(crate) fn begin_native_context_operation(context: usize) -> Option<NativeContextOperation> {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let registration = contexts.get_mut(&context)?;
    if registration.retiring {
        return None;
    }
    registration.active_operations = registration.active_operations.checked_add(1)?;
    Some(NativeContextOperation { context })
}

/// Resolve and pin a shim alias until its native backend call has completed.
/// Context release deactivates aliases, then waits for these operations to drop.
pub(crate) fn acquire_native_card(alias: usize) -> Option<NativeCardOperation> {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    let entry = aliases.get(&alias)?;
    if !entry.active {
        return None;
    }
    let context = entry.context;
    let backend_handle = entry.backend_handle;
    if entry.disconnecting {
        return None;
    }
    let in_flight = entry.in_flight.checked_add(1)?;
    let registration = contexts.get_mut(&context)?;
    if registration.retiring {
        return None;
    }
    let active_operations = registration.active_operations.checked_add(1)?;
    aliases.get_mut(&alias)?.in_flight = in_flight;
    registration.active_operations = active_operations;
    Some(NativeCardOperation {
        alias,
        context,
        backend_handle,
    })
}

/// Run a native card operation through the shared alias mapping while pinning
/// both the alias and owning context for the backend call.
pub(crate) fn with_native_card_operation<T>(
    alias: usize,
    call_backend: impl FnOnce(usize) -> T,
) -> Option<T> {
    let operation = acquire_native_card(alias)?;
    Some(call_backend(operation.backend_handle()))
}

/// Reserve the alias for an exclusive disconnect. This phase is split from
/// waiting so tests can coordinate the in-flight transmit deterministically.
pub(crate) fn reserve_native_card_disconnect(alias: usize) -> Option<NativeCardDisconnect> {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    let entry = aliases.get(&alias)?;
    if !entry.active || entry.disconnecting {
        return None;
    }
    let context = entry.context;
    let backend_handle = entry.backend_handle;
    let registration = contexts.get_mut(&context)?;
    if registration.retiring {
        return None;
    }
    let active_operations = registration.active_operations.checked_add(1)?;
    aliases.get_mut(&alias)?.disconnecting = true;
    registration.active_operations = active_operations;
    Some(NativeCardDisconnect {
        alias,
        context,
        backend_handle,
        finished: false,
    })
}

pub(crate) fn wait_for_native_card_operations(
    disconnect: &NativeCardDisconnect,
    mut on_waiting: impl FnMut(),
) -> bool {
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    let mut waited = false;
    while aliases
        .get(&disconnect.alias)
        .is_some_and(|entry| entry.in_flight != 0)
    {
        waited = true;
        on_waiting();
        aliases = NATIVE_CARD_OPERATIONS_CHANGED
            .get_or_init(Condvar::new)
            .wait(aliases)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    waited
}

pub(crate) fn begin_native_card_disconnect(alias: usize) -> Option<NativeCardDisconnect> {
    let disconnect = reserve_native_card_disconnect(alias)?;
    let _ = wait_for_native_card_operations(&disconnect, || {});
    Some(disconnect)
}

pub(crate) fn finish_native_card_disconnect(mut disconnect: NativeCardDisconnect, succeeded: bool) {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    let context_retiring = contexts
        .get(&disconnect.context)
        .map_or(true, |registration| registration.retiring);
    if succeeded {
        aliases.remove(&disconnect.alias);
        if let Some(registration) = contexts.get_mut(&disconnect.context) {
            registration.aliases.remove(&disconnect.alias);
        }
    } else if let Some(entry) = aliases.get_mut(&disconnect.alias) {
        entry.disconnecting = false;
        if !context_retiring {
            entry.active = true;
        }
    }
    if let Some(registration) = contexts.get_mut(&disconnect.context) {
        registration.active_operations = registration.active_operations.saturating_sub(1);
        if registration.active_operations == 0 {
            NATIVE_CONTEXT_OPERATIONS_CHANGED
                .get_or_init(Condvar::new)
                .notify_all();
        }
    }
    NATIVE_CARD_OPERATIONS_CHANGED
        .get_or_init(Condvar::new)
        .notify_all();
    disconnect.finished = true;
}

impl Drop for NativeCardDisconnect {
    fn drop(&mut self) {
        if !self.finished {
            let mut contexts = lock(&NATIVE_CONTEXTS);
            let mut aliases = lock(&NATIVE_CARD_ALIASES);
            let context_retiring = contexts
                .get(&self.context)
                .map_or(true, |registration| registration.retiring);
            if let Some(entry) = aliases.get_mut(&self.alias) {
                entry.disconnecting = false;
                if !context_retiring {
                    entry.active = true;
                }
            }
            if let Some(registration) = contexts.get_mut(&self.context) {
                registration.active_operations = registration.active_operations.saturating_sub(1);
                if registration.active_operations == 0 {
                    NATIVE_CONTEXT_OPERATIONS_CHANGED
                        .get_or_init(Condvar::new)
                        .notify_all();
                }
            }
            NATIVE_CARD_OPERATIONS_CHANGED
                .get_or_init(Condvar::new)
                .notify_all();
            self.finished = true;
        }
    }
}

/// Dispatch a native disconnect through the same mapping used by the FFI
/// entrypoints; the backend callback receives only its opaque raw handle.
pub(crate) fn with_native_card_disconnect<T>(
    alias: usize,
    call_backend: impl FnOnce(usize) -> T,
    succeeded: impl FnOnce(&T) -> bool,
) -> Option<T> {
    let disconnect = begin_native_card_disconnect(alias)?;
    let result = call_backend(disconnect.backend_handle());
    let success = succeeded(&result);
    finish_native_card_disconnect(disconnect, success);
    Some(result)
}

pub fn finish_native_context_release(mut release: NativeContextRelease, succeeded: bool) {
    release.reacquire_lifecycle_lock();
    if release.registered {
        let mut contexts = lock(&NATIVE_CONTEXTS);
        if succeeded {
            if contexts
                .get(&release.context)
                .is_some_and(|registration| registration.retiring)
            {
                contexts.remove(&release.context);
                let mut aliases = lock(&NATIVE_CARD_ALIASES);
                for alias in &release.aliases {
                    aliases.remove(alias);
                }
            }
        } else if let Some(registration) = contexts.get_mut(&release.context) {
            registration.retiring = false;
            let mut aliases = lock(&NATIVE_CARD_ALIASES);
            for alias in &release.aliases {
                if let Some(entry) = aliases.get_mut(alias) {
                    entry.active = true;
                }
            }
        }
    }
    NATIVE_CONTEXT_OPERATIONS_CHANGED
        .get_or_init(Condvar::new)
        .notify_all();
    release.finished = true;
}

impl Drop for NativeContextRelease {
    fn drop(&mut self) {
        if !self.finished && self.registered {
            self.reacquire_lifecycle_lock();
            let mut contexts = lock(&NATIVE_CONTEXTS);
            if let Some(registration) = contexts.get_mut(&self.context) {
                registration.retiring = false;
                let mut aliases = lock(&NATIVE_CARD_ALIASES);
                for alias in &self.aliases {
                    if let Some(entry) = aliases.get_mut(alias) {
                        entry.active = true;
                    }
                }
            }
            NATIVE_CONTEXT_OPERATIONS_CHANGED
                .get_or_init(Condvar::new)
                .notify_all();
            self.finished = true;
        }
    }
}

pub fn register_native_card_alias(context: usize, backend_handle: usize) -> Option<usize> {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let registration = contexts.get_mut(&context)?;
    if registration.retiring {
        return None;
    }
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    if aliases.len() >= NATIVE_ALIAS_CAPACITY {
        return None;
    }
    for _ in 0..=aliases.len() {
        let serial = NEXT_NATIVE_ALIAS.fetch_add(1, Ordering::Relaxed);
        let alias = NATIVE_ALIAS_BASE | (serial & NATIVE_ALIAS_MASK);
        if !aliases.contains_key(&alias) && !is_synthetic_card_handle_range(alias) {
            aliases.insert(
                alias,
                NativeCardAlias {
                    context,
                    backend_handle,
                    active: true,
                    disconnecting: false,
                    in_flight: 0,
                },
            );
            registration.aliases.insert(alias);
            return Some(alias);
        }
    }
    None
}

/// Compatibility helper retained for older callers that only need to know if
/// a native card registration was accepted. PC/SC shims use the alias-returning
/// variant so the backend handle never crosses the shim boundary.
#[cfg(test)]
pub fn register_native_card(context: usize, backend_handle: usize) -> bool {
    register_native_card_alias(context, backend_handle).is_some()
}

#[cfg(test)]
pub fn resolve_native_card(alias: usize) -> Option<usize> {
    lock(&NATIVE_CARD_ALIASES)
        .get(&alias)
        .filter(|entry| entry.active)
        .map(|entry| entry.backend_handle)
}

#[cfg(test)]
pub fn is_native_card(alias: usize) -> bool {
    resolve_native_card(alias).is_some()
}

/// Shim-issued native card aliases occupy a range distinct from BonCasLink
/// (`0x40000000..0x80000000`) and PX4 (`0x50000000..0x60000000`) handles.
pub fn is_native_alias_handle_range(alias: usize) -> bool {
    (NATIVE_ALIAS_BASE..NATIVE_ALIAS_BASE + NATIVE_ALIAS_CAPACITY).contains(&alias)
}

/// BonCasLink and PX4 synthetic cards share the `0x4...` handle namespace.
pub fn is_synthetic_card_handle_range(handle: usize) -> bool {
    (0x4000_0000..0x8000_0000).contains(&handle)
}

#[cfg(test)]
pub fn unregister_native_card(alias: usize) {
    let mut contexts = lock(&NATIVE_CONTEXTS);
    let mut aliases = lock(&NATIVE_CARD_ALIASES);
    if let Some(entry) = aliases.remove(&alias) {
        if let Some(registration) = contexts.get_mut(&entry.context) {
            registration.aliases.remove(&alias);
        }
    }
}

#[cfg(target_os = "macos")]
fn px4_card_is_reserved(id: usize) -> bool {
    lock(&PX4_CARDS).contains(&id)
}

#[cfg(not(target_os = "macos"))]
fn px4_card_is_reserved(_id: usize) -> bool {
    false
}

/// Reserve an opaque PX4 PC/SC card handle in the shared handle namespace.
#[cfg(target_os = "macos")]
pub(crate) fn reserve_px4_card(id: usize) -> bool {
    let cards = lock(&CARDS);
    let mut px4_cards = lock(&PX4_CARDS);
    if is_native_alias_handle_range(id) || cards.contains_key(&id) {
        return false;
    }
    px4_cards.insert(id)
}

#[cfg(target_os = "macos")]
pub(crate) fn release_px4_card(id: usize) {
    lock(&PX4_CARDS).remove(&id);
}

pub fn normalize_address(value: &str) -> Result<String, String> {
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| "invalid BonCasLink address".to_owned())?;
        let host = &rest[..end];
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("bracketed BonCasLink host must be an IPv6 address".to_owned());
        }
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            6900
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(|| "invalid BonCasLink address".to_owned())?
                .parse::<u16>()
                .map_err(|_| "invalid BonCasLink port".to_owned())?
        };
        (host.to_owned(), port)
    } else {
        match value.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| "invalid BonCasLink port".to_owned())?,
            ),
            Some(_) => return Err("IPv6 BonCasLink addresses must use brackets".to_owned()),
            None => (value.to_owned(), 6900),
        }
    };
    if host.trim().is_empty()
        || port == 0
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("invalid BonCasLink address".to_owned());
    }
    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

fn open_connection_at(address: &str) -> Result<TcpStream, ()> {
    let addresses = address.to_socket_addrs().map_err(|_| ())?;
    for socket in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT) {
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            return Ok(stream);
        }
    }
    Err(())
}

fn begin_card_context_operation(
    owner_context: Option<usize>,
) -> Result<Option<NativeContextOperation>, ()> {
    let Some(context) = owner_context else {
        return Ok(None);
    };
    if let Some(operation) = begin_native_context_operation(context) {
        return Ok(Some(operation));
    }
    if is_dummy_context(context) {
        Ok(None)
    } else {
        Err(())
    }
}

pub fn connect_reader_in_context(context: usize, name: &str) -> Result<usize, ()> {
    if name == BONCASLINK_READER_NAME {
        let address = lock(&SERVER).clone().ok_or(())?;
        return connect_at(&address, Some(context));
    }
    let address = name.strip_prefix("BonCasLink:").ok_or(())?;
    let address = normalize_address(address).map_err(|_| ())?;
    connect_at(&address, Some(context))
}

#[cfg(test)]
fn connect() -> Result<usize, ()> {
    let address = lock(&SERVER).clone().ok_or(())?;
    connect_at(&address, None)
}

#[cfg(test)]
fn connect_reader(name: &str) -> Result<usize, ()> {
    if name == BONCASLINK_READER_NAME {
        return connect();
    }
    let address = name.strip_prefix("BonCasLink:").ok_or(())?;
    let address = normalize_address(address).map_err(|_| ())?;
    connect_at(&address, None)
}

fn connect_at(address: &str, owner_context: Option<usize>) -> Result<usize, ()> {
    let _context_operation = begin_card_context_operation(owner_context)?;
    let stream = open_connection_at(address)?;
    let cards = &mut *lock(&CARDS);
    if let Some(context) = owner_context {
        let dummy_context = lock(&DUMMY_CONTEXTS).contains(&context);
        if _context_operation.is_none() && !dummy_context {
            return Err(());
        }
    }
    for _ in 0..1024 {
        let serial = NEXT_CARD.fetch_add(1, Ordering::Relaxed);
        // Keep IDs positive and representable by macOS's 32-bit SCARDHANDLE.
        let id = 0x4000_0000usize | (serial & 0x3fff_ffff);
        if !cards.contains_key(&id) && !px4_card_is_reserved(id) {
            cards.insert(
                id,
                CardEntry {
                    card: Arc::new(Mutex::new(CardSession {
                        stream: Some(stream),
                        pending_response: None,
                    })),
                    address: address.to_owned(),
                    owner_context,
                },
            );
            return Ok(id);
        }
    }
    Err(())
}

pub fn is_card(id: usize) -> bool {
    lock(&CARDS).contains_key(&id)
}

pub fn disconnect(id: usize) -> bool {
    let owner_context = lock(&CARDS).get(&id).and_then(|entry| entry.owner_context);
    let Ok(_context_operation) = begin_card_context_operation(owner_context) else {
        return false;
    };
    lock(&CARDS).remove(&id).is_some()
}

pub(crate) fn release_context_cards(context: usize) {
    let removed = {
        let mut cards = lock(&CARDS);
        let handles = cards
            .iter()
            .filter_map(|(handle, entry)| (entry.owner_context == Some(context)).then_some(*handle))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|handle| cards.remove(&handle))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

/// Send exactly one BonCasLink length-prefixed APDU. Failed streams are dropped;
/// the next call gets one reconnect attempt before sending.
/// Send an APDU while honoring the receive capacity supplied to SCardTransmit.
/// If the response does not fit, retain it so a retry with the same APDU does
/// not execute a state-changing card command twice. A different APDU discards
/// the abandoned response and proceeds normally.
pub fn transmit_with_capacity(
    id: usize,
    apdu: &[u8],
    response_capacity: usize,
) -> Result<Vec<u8>, CardTransmitError> {
    if apdu.is_empty() || apdu.len() > u8::MAX as usize {
        return Err(CardTransmitError::Unavailable);
    }
    let (card, address, owner_context) = {
        let cards = lock(&CARDS);
        let entry = cards.get(&id).ok_or(CardTransmitError::Unavailable)?;
        (
            Arc::clone(&entry.card),
            entry.address.clone(),
            entry.owner_context,
        )
    };
    let _context_operation =
        begin_card_context_operation(owner_context).map_err(|_| CardTransmitError::Unavailable)?;
    let mut session = card.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(pending) = session.pending_response.as_ref() {
        if pending.request != apdu {
            // The caller abandoned the previous insufficient-buffer retry.
            // Discard its cached response and let the new APDU reach the card.
            session.pending_response = None;
        } else {
            if response_capacity < pending.response.len() {
                return Err(CardTransmitError::InsufficientBuffer(
                    pending.response.len(),
                ));
            }
            return session
                .pending_response
                .take()
                .map(|pending| pending.response)
                .ok_or(CardTransmitError::Unavailable);
        }
    }
    if session.stream.is_none() {
        session.stream =
            Some(open_connection_at(&address).map_err(|_| CardTransmitError::Unavailable)?);
    }
    let response = match exchange(
        session
            .stream
            .as_mut()
            .ok_or(CardTransmitError::Unavailable)?,
        apdu,
    ) {
        Ok(response) => response,
        Err(()) => {
            session.stream = None;
            return Err(CardTransmitError::Unavailable);
        }
    };
    if response_capacity < response.len() {
        let required_length = response.len();
        session.pending_response = Some(PendingResponse {
            request: apdu.to_vec(),
            response,
        });
        return Err(CardTransmitError::InsufficientBuffer(required_length));
    }
    Ok(response)
}

fn exchange(stream: &mut TcpStream, apdu: &[u8]) -> Result<Vec<u8>, ()> {
    stream.write_all(&[apdu.len() as u8]).map_err(|_| ())?;
    stream.write_all(apdu).map_err(|_| ())?;
    let mut size = [0u8; 1];
    stream.read_exact(&mut size).map_err(|_| ())?;
    if size[0] == 0 {
        return Err(());
    }
    let mut response = vec![0u8; size[0] as usize];
    stream.read_exact(&mut response).map_err(|_| ())?;
    Ok(response)
}

/// Construct the PC/SC reader list: the configured (global) virtual reader
/// first, then local readers, then readers registered for specific networks.
///
/// Network-specific readers (`[b25.network_readers]`, e.g. an ACAS server for
/// cable 4K) are opened by name for their NID only. libaribb25 picks the
/// first working reader when none is selected, so listing them first made
/// every other network's ECMs go to that card — B-CAS ECMs sent to an ACAS
/// card, and BS/terrestrial stayed scrambled.
pub fn prepend_reader(mut local: Vec<String>) -> Vec<String> {
    let mut registered = lock(&REGISTERED_READERS)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    registered.sort();
    for name in registered {
        local.retain(|entry| entry != &name);
        local.push(name);
    }
    if enabled() {
        local.retain(|name| name != BONCASLINK_READER_NAME);
        local.insert(0, BONCASLINK_READER_NAME.to_owned());
    }
    local
}

pub fn register_reader(addr: &str) -> Result<String, String> {
    let address = normalize_address(addr)?;
    let name = format!("BonCasLink:{address}");
    lock(&REGISTERED_READERS).insert(name.clone());
    Ok(name)
}

pub fn multi_string(readers: &[String]) -> Vec<u8> {
    if readers.is_empty() {
        return vec![0, 0];
    }
    let mut result = Vec::new();
    for reader in readers {
        result.extend_from_slice(reader.as_bytes());
        result.push(0);
    }
    result.push(0);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn list_connect_transmit_failure_reconnect_and_disconnect() {
        let _guard = super::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let mut len = [0];
            first.read_exact(&mut len).unwrap();
            let mut request = vec![0; len[0] as usize];
            first.read_exact(&mut request).unwrap();
            first.write_all(&[2, 0x90, 0]).unwrap();
            first.read_exact(&mut len).unwrap();
            let mut request = vec![0; len[0] as usize];
            first.read_exact(&mut request).unwrap();
            first.write_all(&[0]).unwrap();
            drop(first);

            let (mut second, _) = listener.accept().unwrap();
            second.read_exact(&mut len).unwrap();
            let mut request = vec![0; len[0] as usize];
            second.read_exact(&mut request).unwrap();
            second.write_all(&[2, 0x90, 0]).unwrap();
        });

        set_server(Some(&address.to_string())).unwrap();
        let readers = prepend_reader(vec!["Local Reader".to_owned()]);
        assert_eq!(
            readers.first().map(String::as_str),
            Some(BONCASLINK_READER_NAME)
        );
        assert!(readers.contains(&"Local Reader".to_owned()));
        assert!(multi_string(&readers).starts_with(b"BonCasLink Client Card Reader\0"));
        let id = connect().unwrap();
        assert_eq!(
            transmit_with_capacity(id, &[0x00, 0x84], usize::MAX).unwrap(),
            [0x90, 0]
        );
        assert!(transmit_with_capacity(id, &[0x00, 0x84], usize::MAX).is_err());
        assert_eq!(
            transmit_with_capacity(id, &[0x00, 0x84], usize::MAX).unwrap(),
            [0x90, 0]
        );
        assert!(transmit_with_capacity(id, &[0; 256], usize::MAX).is_err());
        assert!(disconnect(id));
        assert!(!is_card(id));
        server.join().unwrap();
        set_server(None).unwrap();
    }

    #[test]
    fn validates_address_and_defaults_port() {
        assert_eq!(
            normalize_address("cards.example").unwrap(),
            "cards.example:6900"
        );
        assert_eq!(normalize_address("[::1]").unwrap(), "[::1]:6900");
        assert!(normalize_address("host:0").is_err());
        assert!(normalize_address("2001:db8::1").is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn px4_handle_reservation_rejects_native_aliases() {
        let context = 0x3000_0101;
        let backend_handle = 0x3000_4001;
        register_native_context(context);
        let alias = register_native_card_alias(context, backend_handle).unwrap();
        assert!(is_native_alias_handle_range(alias));
        assert!(!reserve_px4_card(alias));
        unregister_native_context(context);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn native_raw_handle_may_match_px4_namespace_because_alias_is_exposed() {
        let context = 0x5100_0102;
        let backend_handle = 0x5123_4002;
        register_native_context(context);
        assert!(reserve_px4_card(backend_handle));
        let alias = register_native_card_alias(context, backend_handle).unwrap();
        assert_ne!(alias, backend_handle);
        assert_eq!(resolve_native_card(alias), Some(backend_handle));
        release_px4_card(backend_handle);
        unregister_native_context(context);
    }

    #[test]
    fn native_backend_handle_in_synthetic_range_is_wrapped_in_alias() {
        let context = 0x3000_0105;
        let backend_handle = 0x4fff_fffe;
        register_native_context(context);

        let alias = register_native_card_alias(context, backend_handle).unwrap();
        assert_ne!(alias, backend_handle);
        assert!(is_native_alias_handle_range(alias));
        assert_eq!(resolve_native_card(alias), Some(backend_handle));

        unregister_native_context(context);
    }

    #[test]
    fn native_context_release_removes_its_card_aliases() {
        let _guard = super::test_lock();
        let context = 0x3000_0101;
        let backend_handle = 0x3000_0201;
        register_native_context(context);
        let alias = register_native_card_alias(context, backend_handle).unwrap();

        unregister_native_context(context);

        assert_eq!(resolve_native_card(alias), None);
        assert!(is_native_alias_handle_range(alias));
    }

    #[test]
    fn native_context_registration_rejects_a_live_dummy_context() {
        let _guard = super::test_lock();
        let context = new_dummy_context();
        assert_ne!(context, 0);

        assert!(!register_native_context(context));
        assert!(!lock(&NATIVE_CONTEXTS).contains_key(&context));
        assert!(release_dummy_context(context));
    }

    #[test]
    fn release_wait_does_not_block_other_context_registration_or_same_id_guard() {
        let _guard = super::test_lock();
        let context_a = 0x3000_0111;
        let context_b = 0x3000_0112;
        assert!(register_native_context(context_a));
        let operation = begin_native_context_operation(context_a).unwrap();

        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let release_thread = std::thread::spawn(move || {
            let release = begin_native_context_release_with(context_a, || {
                let _ = waiting_tx.send(());
            });
            release_tx.send(release.was_registered()).unwrap();
            let succeeded = finish_rx.recv().unwrap();
            finish_native_context_release(release, succeeded);
        });

        waiting_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(register_native_context(context_b));
        assert!(!register_native_context(context_a));
        let duplicate_release = begin_native_context_release(context_a);
        assert!(!duplicate_release.was_registered());
        finish_native_context_release(duplicate_release, false);

        drop(operation);
        assert!(release_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());
        finish_tx.send(false).unwrap();
        release_thread.join().unwrap();

        assert!(!register_native_context(context_a));
        unregister_native_context(context_a);
        assert!(register_native_context(context_a));
        unregister_native_context(context_a);
        unregister_native_context(context_b);
    }

    #[test]
    fn native_card_registration_requires_a_live_context() {
        let _guard = super::test_lock();
        assert!(!register_native_card(0x3000_0106, 0x4fff_fffd));
        assert_eq!(register_native_card_alias(0x3000_0106, 0x4fff_fffd), None);
        assert!(!lock(&NATIVE_CONTEXTS).contains_key(&0x3000_0106));
    }

    #[test]
    fn native_card_operation_pins_its_backend_handle_until_drop() {
        let _guard = super::test_lock();
        let context = 0x3000_0109;
        let backend_handle = 0xF123_4567;
        assert!(register_native_context(context));
        let alias = register_native_card_alias(context, backend_handle).unwrap();

        let operation = acquire_native_card(alias).unwrap();
        assert_eq!(operation.backend_handle(), backend_handle);
        assert_eq!(lock(&NATIVE_CONTEXTS)[&context].active_operations, 1);
        drop(operation);
        assert_eq!(lock(&NATIVE_CONTEXTS)[&context].active_operations, 0);
        unregister_native_context(context);
    }

    #[test]
    fn shared_native_dispatch_helpers_translate_aliases_and_reject_stale_values() {
        let _guard = super::test_lock();
        let context = 0x3000_0110;
        let backend_handle = 0xF123_4567;
        assert!(register_native_context(context));
        let alias = register_native_card_alias(context, backend_handle).unwrap();
        assert_ne!(alias, backend_handle);

        let transmitted = with_native_card_operation(alias, |raw_handle| {
            assert_eq!(raw_handle, backend_handle);
            assert_ne!(raw_handle, alias);
            raw_handle
        });
        assert_eq!(transmitted, Some(backend_handle));

        let disconnected = with_native_card_disconnect(
            alias,
            |raw_handle| {
                assert_eq!(raw_handle, backend_handle);
                assert_ne!(raw_handle, alias);
                raw_handle
            },
            |raw_handle| *raw_handle == backend_handle,
        );
        assert_eq!(disconnected, Some(backend_handle));
        assert!(is_native_alias_handle_range(alias));
        assert!(with_native_card_operation(alias, |_| panic!("stale alias dispatched")).is_none());
        assert!(with_native_card_disconnect(
            alias,
            |_| panic!("stale alias disconnected"),
            |_| true
        )
        .is_none());
        unregister_native_context(context);
    }

    #[test]
    fn disconnect_waits_for_same_alias_transmit_and_reopens_only_on_failure() {
        let _guard = super::test_lock();
        let context = 0x3000_0111;
        let backend_handle = 0x4000_1111;
        let other_backend_handle = 0x7000_2222;
        assert!(register_native_context(context));
        let alias = register_native_card_alias(context, backend_handle).unwrap();
        let other_alias = register_native_card_alias(context, other_backend_handle).unwrap();

        let (transmit_started_tx, transmit_started_rx) = std::sync::mpsc::channel();
        let (finish_transmit_tx, finish_transmit_rx) = std::sync::mpsc::channel();
        let transmit_thread = std::thread::spawn(move || {
            with_native_card_operation(alias, |raw_handle| {
                transmit_started_tx.send(raw_handle).unwrap();
                finish_transmit_rx.recv().unwrap();
                raw_handle
            })
            .unwrap()
        });
        assert_eq!(transmit_started_rx.recv().unwrap(), backend_handle);

        let disconnect = reserve_native_card_disconnect(alias).unwrap();
        assert_eq!(disconnect.backend_handle(), backend_handle);
        assert!(with_native_card_operation(alias, |_| 0).is_none());
        assert_eq!(
            with_native_card_operation(other_alias, |raw_handle| raw_handle),
            Some(other_backend_handle),
            "disconnecting one card must not block other aliases"
        );

        let (wait_started_tx, wait_started_rx) = std::sync::mpsc::channel();
        let (allow_wait_tx, allow_wait_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut announced_wait = false;
            let waited = wait_for_native_card_operations(&disconnect, || {
                if !announced_wait {
                    announced_wait = true;
                    wait_started_tx.send(()).unwrap();
                    allow_wait_rx.recv().unwrap();
                }
            });
            finish_native_card_disconnect(disconnect, false);
            waited
        });
        wait_started_rx.recv().unwrap();
        allow_wait_tx.send(()).unwrap();
        // This call acquires the alias lock after the waiter atomically enters
        // Condvar::wait, so the result proves disconnect reservation is active.
        assert!(with_native_card_operation(alias, |_| 0).is_none());
        finish_transmit_tx.send(()).unwrap();

        assert_eq!(transmit_thread.join().unwrap(), backend_handle);
        assert!(
            waiter.join().unwrap(),
            "disconnect must have waited for transmit"
        );
        assert_eq!(
            with_native_card_operation(alias, |raw| raw),
            Some(backend_handle)
        );

        let disconnected = with_native_card_disconnect(
            alias,
            |raw| {
                assert_eq!(raw, backend_handle);
                raw
            },
            |raw| *raw == backend_handle,
        );
        assert_eq!(disconnected, Some(backend_handle));
        assert!(with_native_card_operation(alias, |_| 0).is_none());
        unregister_native_context(context);
    }

    #[test]
    fn context_release_waits_for_disconnect_lease_without_premature_reactivation() {
        let _guard = super::test_lock();
        let context = 0x3000_0112;
        let backend_handle = 0x5000_1122;
        assert!(register_native_context(context));
        let alias = register_native_card_alias(context, backend_handle).unwrap();
        let disconnect = reserve_native_card_disconnect(alias).unwrap();

        let (release_waiting_tx, release_waiting_rx) = std::sync::mpsc::channel();
        let (allow_release_wait_tx, allow_release_wait_rx) = std::sync::mpsc::channel();
        let (release_ready_tx, release_ready_rx) = std::sync::mpsc::channel();
        let (finish_release_tx, finish_release_rx) = std::sync::mpsc::channel();
        let release_thread = std::thread::spawn(move || {
            let mut announced_wait = false;
            let release = begin_native_context_release_with(context, || {
                if !announced_wait {
                    announced_wait = true;
                    release_waiting_tx.send(()).unwrap();
                    allow_release_wait_rx.recv().unwrap();
                }
            });
            release_ready_tx
                .send(release.waited_for_operations)
                .unwrap();
            finish_release_rx.recv().unwrap();
            finish_native_context_release(release, false);
        });

        release_waiting_rx.recv().unwrap();
        allow_release_wait_tx.send(()).unwrap();
        // Completing the disconnect releases the context operation count. The
        // context retirement must not be released by the failed disconnect.
        finish_native_card_disconnect(disconnect, false);
        assert!(release_ready_rx.recv().unwrap());
        {
            let contexts = lock(&NATIVE_CONTEXTS);
            assert!(contexts[&context].retiring);
            assert_eq!(contexts[&context].active_operations, 0);
            let aliases = lock(&NATIVE_CARD_ALIASES);
            let entry = &aliases[&alias];
            assert!(!entry.active);
            assert!(!entry.disconnecting);
        }
        assert!(with_native_card_operation(alias, |_| 0).is_none());

        finish_release_tx.send(()).unwrap();
        release_thread.join().unwrap();
        assert_eq!(
            with_native_card_operation(alias, |raw| raw),
            Some(backend_handle)
        );
        unregister_native_context(context);
    }

    #[test]
    fn failed_native_context_release_restores_card_alias() {
        let _guard = super::test_lock();
        let context = 0x3000_0107;
        register_native_context(context);
        let alias = register_native_card_alias(context, 0x4000_1234).unwrap();

        let release = begin_native_context_release(context);
        assert!(release.was_registered());
        assert_eq!(resolve_native_card(alias), None);
        assert_eq!(register_native_card_alias(context, 0x4000_5678), None);
        finish_native_context_release(release, false);

        assert_eq!(resolve_native_card(alias), Some(0x4000_1234));
        let next_alias = register_native_card_alias(context, 0x4000_5678).unwrap();
        assert_eq!(resolve_native_card(next_alias), Some(0x4000_5678));
        unregister_native_context(context);
    }

    #[test]
    fn successful_release_allows_context_id_reuse_without_reviving_old_alias() {
        let _guard = super::test_lock();
        let context = 0x3000_0108;
        register_native_context(context);
        let old_alias = register_native_card_alias(context, 0x4000_1235).unwrap();

        let release = begin_native_context_release(context);
        finish_native_context_release(release, true);
        assert_eq!(resolve_native_card(old_alias), None);
        assert!(is_native_alias_handle_range(old_alias));
        assert!(acquire_native_card(old_alias).is_none());

        assert!(register_native_context(context));
        let new_alias = register_native_card_alias(context, 0x4000_6789).unwrap();
        assert_ne!(new_alias, old_alias);
        assert_eq!(resolve_native_card(old_alias), None);
        assert_eq!(resolve_native_card(new_alias), Some(0x4000_6789));
        unregister_native_context(context);
    }

    #[test]
    fn duplicate_backend_card_handles_get_distinct_aliases() {
        let _guard = super::test_lock();
        let card = 0x3000_0202;
        let context = 0x3000_0102;
        register_native_context(context);
        let first_alias = register_native_card_alias(context, card).unwrap();
        let second_alias = register_native_card_alias(context, card).unwrap();
        unregister_native_card(first_alias);
        let first_is_gone = resolve_native_card(first_alias).is_none();
        let second_still_resolves = resolve_native_card(second_alias) == Some(card);
        unregister_native_card(second_alias);
        unregister_native_context(context);

        assert_ne!(first_alias, second_alias);
        assert!(first_is_gone);
        assert!(second_still_resolves);
    }

    #[test]
    fn same_backend_handle_can_have_aliases_in_distinct_contexts() {
        let _guard = super::test_lock();
        let first_context = 0x3000_0103;
        let second_context = 0x3000_0104;
        let card = 0x3000_0203;
        register_native_context(first_context);
        register_native_context(second_context);
        let first_alias = register_native_card_alias(first_context, card).unwrap();
        let second_alias = register_native_card_alias(second_context, card).unwrap();
        assert_ne!(first_alias, second_alias);

        unregister_native_context(first_context);
        let remained_after_first_context_release = resolve_native_card(second_alias) == Some(card);
        unregister_native_context(second_context);
        let remained_after_last_context_release = resolve_native_card(second_alias).is_some();

        assert!(remained_after_first_context_release);
        assert!(!remained_after_last_context_release);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn px4_handle_reservation_rejects_existing_boncaslink_card_handles() {
        let id = 0x5123_4003;
        lock(&CARDS).insert(
            id,
            CardEntry {
                card: Arc::new(Mutex::new(CardSession {
                    stream: None,
                    pending_response: None,
                })),
                address: "127.0.0.1:6900".to_owned(),
                owner_context: None,
            },
        );

        assert!(!reserve_px4_card(id));

        lock(&CARDS).remove(&id);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn boncaslink_allocation_skips_px4_reserved_handles() {
        let _guard = super::test_lock();
        let serial = 0x1000_0990;
        let id = 0x4000_0000usize | serial;
        let previous_serial = NEXT_CARD.swap(serial, Ordering::Relaxed);
        assert!(reserve_px4_card(id));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });

        let card = connect_at(&address.to_string(), None).unwrap();

        assert_ne!(card, id);
        disconnect(card);
        release_px4_card(id);
        NEXT_CARD.store(previous_serial, Ordering::Relaxed);
        server.join().unwrap();
    }

    #[test]
    fn registered_reader_names_route_to_their_own_servers() {
        let _guard = super::test_lock();
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = TcpListener::bind("127.0.0.1:0").unwrap();
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();
        let serve = |listener: TcpListener, response: u8| {
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0];
                stream.read_exact(&mut len).unwrap();
                let mut request = vec![0; len[0] as usize];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(&[1, response]).unwrap();
            })
        };
        let server_a = serve(a, 0xA1);
        let server_b = serve(b, 0xB2);
        let name_a = register_reader(&a_addr.to_string()).unwrap();
        let name_b = register_reader(&b_addr.to_string()).unwrap();
        assert_ne!(name_a, name_b);
        let names = prepend_reader(Vec::new());
        assert!(names.contains(&name_a));
        assert!(names.contains(&name_b));
        let card_a = connect_reader(&name_a).unwrap();
        let card_b = connect_reader(&name_b).unwrap();
        assert_eq!(
            transmit_with_capacity(card_a, &[0x01], usize::MAX).unwrap(),
            [0xA1]
        );
        assert_eq!(
            transmit_with_capacity(card_b, &[0x01], usize::MAX).unwrap(),
            [0xB2]
        );
        disconnect(card_a);
        disconnect(card_b);
        server_a.join().unwrap();
        server_b.join().unwrap();
    }
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
