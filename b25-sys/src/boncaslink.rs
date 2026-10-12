//! BonCasLink virtual PC/SC reader shared by the platform shims.
//! Called across a C ABI boundary: all state access recovers poisoned locks.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

pub const BONCASLINK_READER_NAME: &str = "BonCasLink Client Card Reader";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

static SERVER: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// One entry per virtual card handle. Each stream has its own lock so that a
/// slow or dead server only stalls the decoder that is talking to it: the map
/// lock is also taken by `is_card`, which every local-reader call goes through.
type Card = Arc<Mutex<Option<TcpStream>>>;
type CardEntry = (Card, String);
static CARDS: OnceLock<Mutex<HashMap<usize, CardEntry>>> = OnceLock::new();
static REGISTERED_READERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static DUMMY_CONTEXTS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
static NATIVE_CARDS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
static NATIVE_CONTEXTS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
static NEXT_CARD: AtomicUsize = AtomicUsize::new(1);
static NEXT_CONTEXT: AtomicUsize = AtomicUsize::new(0x1000);

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
        if id != 0 && !contexts.contains(&id) && !lock(&NATIVE_CONTEXTS).contains(&id) {
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
    lock(&DUMMY_CONTEXTS).remove(&id)
}

pub fn register_native_context(id: usize) {
    lock(&NATIVE_CONTEXTS).insert(id);
}

pub fn unregister_native_context(id: usize) {
    lock(&NATIVE_CONTEXTS).remove(&id);
}

pub fn register_native_card(id: usize) {
    lock(&NATIVE_CARDS).insert(id);
}

pub fn unregister_native_card(id: usize) {
    lock(&NATIVE_CARDS).remove(&id);
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

pub fn connect() -> Result<usize, ()> {
    let address = lock(&SERVER).clone().ok_or(())?;
    connect_at(&address)
}

pub fn connect_reader(name: &str) -> Result<usize, ()> {
    if name == BONCASLINK_READER_NAME {
        return connect();
    }
    let address = name.strip_prefix("BonCasLink:").ok_or(())?;
    let address = normalize_address(address).map_err(|_| ())?;
    connect_at(&address)
}

fn connect_at(address: &str) -> Result<usize, ()> {
    let stream = open_connection_at(address)?;
    let native_cards = lock(&NATIVE_CARDS);
    let cards = &mut *lock(&CARDS);
    for _ in 0..1024 {
        let serial = NEXT_CARD.fetch_add(1, Ordering::Relaxed);
        // Keep IDs positive and representable by macOS's 32-bit SCARDHANDLE.
        let id = 0x4000_0000usize | (serial & 0x3fff_ffff);
        if !cards.contains_key(&id) && !native_cards.contains(&id) {
            cards.insert(id, (Arc::new(Mutex::new(Some(stream))), address.to_owned()));
            return Ok(id);
        }
    }
    Err(())
}

pub fn is_card(id: usize) -> bool {
    lock(&CARDS).contains_key(&id)
}

pub fn disconnect(id: usize) -> bool {
    lock(&CARDS).remove(&id).is_some()
}

/// Send exactly one BonCasLink length-prefixed APDU. Failed streams are dropped;
/// the next call gets one reconnect attempt before sending.
pub fn transmit(id: usize, apdu: &[u8]) -> Result<Vec<u8>, ()> {
    if apdu.is_empty() || apdu.len() > u8::MAX as usize {
        return Err(());
    }
    let (card, address) = lock(&CARDS).get(&id).cloned().ok_or(())?;
    let mut stream = card.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if stream.is_none() {
        *stream = Some(open_connection_at(&address)?);
    }
    match exchange(stream.as_mut().ok_or(())?, apdu) {
        Ok(response) => Ok(response),
        Err(()) => {
            *stream = None;
            Err(())
        }
    }
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

/// Construct the PC/SC multi-string with the virtual reader first.
pub fn prepend_reader(mut local: Vec<String>) -> Vec<String> {
    let registered = lock(&REGISTERED_READERS)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    for name in registered.into_iter().rev() {
        local.retain(|entry| entry != &name);
        local.insert(0, name);
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
        assert_eq!(transmit(id, &[0x00, 0x84]).unwrap(), [0x90, 0]);
        assert!(transmit(id, &[0x00, 0x84]).is_err());
        assert_eq!(transmit(id, &[0x00, 0x84]).unwrap(), [0x90, 0]);
        assert!(transmit(id, &[0; 256]).is_err());
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
        assert_eq!(transmit(card_a, &[0x01]).unwrap(), [0xA1]);
        assert_eq!(transmit(card_b, &[0x01]).unwrap(), [0xB2]);
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
