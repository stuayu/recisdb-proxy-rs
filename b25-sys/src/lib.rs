use log::info;
use std::cell::Cell;
use std::io::{Error, Read, Write};
use std::sync::OnceLock;
use std::sync::{Mutex, MutexGuard};

use crate::bindings::InnerDecoder;

#[cfg(feature = "block00cbc")]
mod access_control;
mod bindings;
mod boncaslink;
#[cfg(unix)]
mod pcsc_shim;
#[cfg(windows)]
mod pcsc_windows;

static KEY0: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static KEY1: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static DECODER_CREATION: OnceLock<Mutex<()>> = OnceLock::new();
#[cfg(feature = "prioritized_card_reader")]
static DEFAULT_CARD_READER: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn decoder_creation_lock() -> MutexGuard<'static, ()> {
    DECODER_CREATION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Hand a reader-name pattern to libaribb25.
///
/// The C side takes `const TCHAR *`. libaribb25 is built with `UNICODE` on
/// Windows, so there `TCHAR` is `wchar_t` and the name must be UTF-16. Passing
/// an 8-bit string makes C read past its end: an empty name turns into
/// trailing heap bytes, no reader matches that garbage, and every decoder fails
/// with `ALL_READERS_CONNECTION_FAILED`.
///
/// Returns `false` for an interior NUL or a name libaribb25 rejects.
#[cfg(feature = "prioritized_card_reader")]
fn override_reader_pattern(name: &str) -> bool {
    if name.contains('\0') {
        return false;
    }
    #[cfg(windows)]
    let encoded: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    #[cfg(not(windows))]
    let encoded: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
    unsafe { crate::bindings::override_card_reader_name_pattern(encoded.as_ptr().cast()) == 0 }
}

/// Set keys so that ECM/EMM could be interpreted by StreamDecoder.
pub fn set_keys(key0: Vec<u64>, key1: Vec<u64>) {
    KEY0.lock().unwrap().clear();
    KEY0.lock().unwrap().extend(key0);
    KEY1.lock().unwrap().clear();
    KEY1.lock().unwrap().extend(key1);
}

/// Tell libaribb25 which card reader to use, by exact name.
///
/// Without this, libaribb25 walks every reader the PC/SC daemon reports and
/// tries to talk to each one in turn. On a machine with a non-B-CAS reader
/// attached (a bank-card/EMV reader, say) that costs several seconds per
/// decoder start, and the first reader to answer wins even if it is the wrong
/// one. Pass a name from [`list_card_readers`] to skip straight to it.
///
/// The name must match what PC/SC reports exactly; `false` means libaribb25
/// rejected it (1024 characters or longer). An empty name restores automatic
/// probing across all readers.
///
/// Takes `&str` but hands C a NUL-terminated copy — the C side runs `strlen`
/// on it, and a bare `str::as_ptr()` is not NUL-terminated. An interior NUL
/// makes this return `false` rather than silently truncating.
#[cfg(feature = "prioritized_card_reader")]
pub fn set_card_reader_name(name: &str) -> bool {
    let _guard = decoder_creation_lock();
    if !override_reader_pattern(name) {
        return false;
    }
    let value = name.to_owned();
    *DEFAULT_CARD_READER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = if value.is_empty() { None } else { Some(value) };
    true
}

#[cfg(not(feature = "prioritized_card_reader"))]
pub fn set_card_reader_name(_name: &str) -> bool {
    false
}

pub use crate::pcsc_readers::list_card_readers;
mod pcsc_readers;

/// Name exposed for the BonCasLink virtual card reader.
pub const BONCASLINK_READER_NAME: &str = boncaslink::BONCASLINK_READER_NAME;

/// Configure the BonCasLink smart-card server used by the virtual PC/SC reader.
/// An empty address or `None` disables the virtual reader; an omitted port uses 6900.
pub fn set_boncaslink_server(addr: Option<&str>) -> Result<(), String> {
    boncaslink::set_server(addr)
}

/// Validate and normalize a BonCasLink server address.
pub fn normalize_boncaslink_address(addr: &str) -> Result<String, String> {
    boncaslink::normalize_address(addr)
}

/// Register an address-specific virtual reader and return its PC/SC reader name.
pub fn register_boncaslink_reader(addr: &str) -> Result<String, String> {
    boncaslink::register_reader(addr)
}

/// Decode ARIB-STD-B25 stream with libaribb25. Both `Read` and `Write` are implemented.
pub struct StreamDecoder {
    received: Cell<usize>,
    sent: Cell<usize>,
    inner: Mutex<InnerDecoder>,
}
impl Drop for StreamDecoder {
    fn drop(&mut self) {
        info!(
            "Decoder: {}B received, and {}B converted.",
            self.received.get(),
            self.sent.get()
        );
    }
}

pub struct DecoderOptions {
    pub enable_working_key: bool,
    pub round: i32,
    pub strip: bool,
    pub emm: bool,
    pub simd: bool,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            enable_working_key: false,
            round: 4,
            strip: true,
            emm: false,
            simd: true,
        }
    }
}

impl StreamDecoder {
    pub fn new(opt: DecoderOptions) -> Result<Self, Error> {
        Self::with_card_reader(opt, None)
    }

    /// Construct a decoder using a reader name only for this decoder instance.
    /// libaribb25 connects during `InnerDecoder::new`, while the temporary
    /// process-global pattern is protected by `DECODER_CREATION`.
    pub fn with_card_reader(opt: DecoderOptions, reader: Option<&str>) -> Result<Self, Error> {
        let _guard = decoder_creation_lock();
        #[cfg(feature = "prioritized_card_reader")]
        let default = DEFAULT_CARD_READER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        #[cfg(feature = "prioritized_card_reader")]
        {
            let selected = reader.or(default.as_deref()).unwrap_or("");
            if !override_reader_pattern(selected) {
                return Err(Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid card reader name",
                ));
            }
        }
        #[cfg(not(feature = "prioritized_card_reader"))]
        if reader.is_some() {
            return Err(Error::new(
                std::io::ErrorKind::Unsupported,
                "b25-sys built without prioritized_card_reader feature",
            ));
        }
        let inner_result = unsafe {
            InnerDecoder::new(opt.enable_working_key).map(|inner| {
                // Set options to the decoder
                inner.dec.as_ref().set_multi2_round(opt.round);
                inner.dec.as_ref().set_strip(if opt.strip { 1 } else { 0 });
                inner.dec.as_ref().set_emm_proc(if opt.emm { 1 } else { 0 });
                inner
                    .dec
                    .as_ref()
                    .set_simd_mode(if opt.simd { 3 } else { 0 });
                inner
            })
        };

        // Restore the process default immediately after construction. The
        // decoder retains its connected card; libaribb25 has no later reconnect
        // path (the only other connect_card calls are disabled legacy code).
        #[cfg(feature = "prioritized_card_reader")]
        {
            let _ = override_reader_pattern(default.as_deref().unwrap_or(""));
        }
        let inner = inner_result?;

        Ok(Self {
            received: Cell::new(0),
            sent: Cell::new(0),
            inner: Mutex::new(inner),
        })
    }
}

#[cfg(test)]
mod decoder_creation_tests {
    use super::decoder_creation_lock;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn decoder_creation_lock_serializes_pattern_scopes_without_a_card() {
        let guard = decoder_creation_lock();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let other = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = decoder_creation_lock();
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        other.join().unwrap();
    }
}

impl Read for StreamDecoder {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().read(buf).map(|value| {
            self.sent.set(self.sent.get() + value);
            value
        })
    }
}

impl Write for StreamDecoder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().write(buf).map(|value| {
            self.received.set(self.received.get() + value);
            value
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap().flush()
    }
}
