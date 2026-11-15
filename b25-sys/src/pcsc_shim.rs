//! Runtime PC/SC backend selection for Unix.
//!
//! libaribb25 (statically linked) references six SCard* functions. Instead of
//! putting `libpcsclite.so.1` into DT_NEEDED at link time, this module defines
//! those symbols itself and forwards them to a backend chosen at runtime via
//! dlopen, in this order:
//!
//! 1. the library named by the `B25_PCSC_LIB` environment variable (if set)
//! 2. libraries next to the executable
//! 3. libraries found by the system
//! 4. the macOS PCSC.framework binary
//!
//! If no backend can be loaded, every entry point returns SCARD_E_NO_SERVICE,
//! which libaribb25 reports as a card-reader initialization failure.
//!
//! These functions are called from C; they must never panic.

use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::OnceLock;

use crate::boncaslink;

#[cfg(target_os = "macos")]
mod ffi_types {
    pub type Dword = u32;
    pub type Long = i32;
    pub type ScardContext = i32;
}

#[cfg(all(unix, not(target_os = "macos")))]
mod ffi_types {
    use std::ffi::{c_long, c_ulong};

    pub type Dword = c_ulong;
    pub type Long = c_long;
    pub type ScardContext = c_long;
}

use ffi_types::{Dword, Long, ScardContext};

// "The Smart card resource manager is not running."
const SCARD_E_NO_SERVICE: Long = 0x8010_001Du32 as Long;
const SCARD_E_READER_UNAVAILABLE: Long = 0x8010_0017u32 as Long;
const SCARD_E_INSUFFICIENT_BUFFER: Long = 0x8010_0008u32 as Long;
const SCARD_E_INVALID_PARAMETER: Long = 0x8010_0004u32 as Long;
const SCARD_E_INVALID_HANDLE: Long = 0x8010_0003u32 as Long;
#[cfg(target_os = "macos")]
const SCARD_E_NO_SMARTCARD: Long = 0x8010_000Cu32 as Long;
const SCARD_E_NOT_TRANSACTED: Long = 0x8010_0016u32 as Long;

const RTLD_NOW: c_int = 2;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

// Types mirror the PC/SC ABI on each Unix platform.
type EstablishFn =
    unsafe extern "C" fn(Dword, *const c_void, *const c_void, *mut ScardContext) -> Long;
type ReleaseFn = unsafe extern "C" fn(ScardContext) -> Long;
type ListReadersFn =
    unsafe extern "C" fn(ScardContext, *const c_char, *mut c_char, *mut Dword) -> Long;
type ConnectFn = unsafe extern "C" fn(
    ScardContext,
    *const c_char,
    Dword,
    Dword,
    *mut ScardContext,
    *mut Dword,
) -> Long;
type DisconnectFn = unsafe extern "C" fn(ScardContext, Dword) -> Long;
type TransmitFn = unsafe extern "C" fn(
    ScardContext,
    *const c_void,
    *const u8,
    Dword,
    *mut c_void,
    *mut u8,
    *mut Dword,
) -> Long;

/// pcsclite's SCARD_IO_REQUEST. libaribb25 references the pcsclite-exported
/// constants g_rgSCard*Pci (via the SCARD_PCI_* macros) as data symbols, which
/// cannot be forwarded through dlsym. Their contents are fixed protocol
/// descriptors, so we export identical copies ourselves; backends only read
/// the pointed-to values.
#[repr(C)]
pub struct ScardIoRequest {
    dw_protocol: Dword,
    cb_pci_length: Dword,
}

const PCI_LEN: Dword = std::mem::size_of::<ScardIoRequest>() as Dword;

#[allow(non_upper_case_globals)]
#[no_mangle]
pub static g_rgSCardT0Pci: ScardIoRequest = ScardIoRequest {
    dw_protocol: 1, // SCARD_PROTOCOL_T0
    cb_pci_length: PCI_LEN,
};
#[allow(non_upper_case_globals)]
#[no_mangle]
pub static g_rgSCardT1Pci: ScardIoRequest = ScardIoRequest {
    dw_protocol: 2, // SCARD_PROTOCOL_T1
    cb_pci_length: PCI_LEN,
};
#[allow(non_upper_case_globals)]
#[no_mangle]
pub static g_rgSCardRawPci: ScardIoRequest = ScardIoRequest {
    dw_protocol: 4, // SCARD_PROTOCOL_RAW
    cb_pci_length: PCI_LEN,
};

struct PcscBackend {
    establish_context: EstablishFn,
    release_context: ReleaseFn,
    list_readers: ListReadersFn,
    connect: ConnectFn,
    disconnect: DisconnectFn,
    transmit: TransmitFn,
}

static BACKEND: OnceLock<Option<PcscBackend>> = OnceLock::new();

fn mac_handle_key(handle: i32) -> usize {
    handle as u32 as usize
}

fn mac_handle_from_key(key: usize) -> i32 {
    key as u32 as i32
}

#[cfg(target_os = "macos")]
fn scard_handle_key(handle: ScardContext) -> usize {
    mac_handle_key(handle)
}

#[cfg(not(target_os = "macos"))]
fn scard_handle_key(handle: ScardContext) -> usize {
    handle as usize
}

#[cfg(target_os = "macos")]
fn scard_handle_from_key(key: usize) -> ScardContext {
    mac_handle_from_key(key)
}

#[cfg(not(target_os = "macos"))]
fn scard_handle_from_key(key: usize) -> ScardContext {
    key as ScardContext
}

#[cfg(all(test, target_os = "macos"))]
std::thread_local! {
    static PX4_READER_OVERRIDE: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "macos")]
fn px4_readers() -> Vec<String> {
    #[cfg(test)]
    if let Some(readers) = PX4_READER_OVERRIDE.with(|readers| readers.borrow().clone()) {
        return readers;
    }
    crate::px4_card::list_readers()
}

#[cfg(not(target_os = "macos"))]
fn px4_readers() -> Vec<String> {
    Vec::new()
}

/// Order: configured BonCasLink reader, local PC/SC readers, PX4 built-in
/// readers, then network-specific BonCasLink readers (`BonCasLink:<addr>`),
/// which must never become the default card (see `boncaslink::prepend_reader`).
fn merge_px4_readers(px4: Vec<String>, mut readers: Vec<String>) -> Vec<String> {
    let boncaslink_count = readers
        .iter()
        .take_while(|reader| reader.as_str() == boncaslink::BONCASLINK_READER_NAME)
        .count();
    let mut merged = Vec::with_capacity(readers.len() + px4.len());
    merged.extend(readers.drain(..boncaslink_count));
    let (network_specific, rest): (Vec<String>, Vec<String>) = readers
        .into_iter()
        .partition(|reader| reader.starts_with("BonCasLink:"));
    merged.extend(rest.into_iter().filter(|existing| !px4.contains(existing)));
    for reader in px4.into_iter().chain(network_specific) {
        if !merged.contains(&reader) {
            merged.push(reader);
        }
    }
    merged
}

fn virtual_readers_after_native_result(
    native_status: Option<Long>,
    boncaslink_available: bool,
    probe_px4: impl FnOnce() -> bool,
) -> bool {
    if native_status == Some(0) {
        return false;
    }
    boncaslink_available || probe_px4()
}

fn load_candidate(name: &str) -> Option<PcscBackend> {
    let c_name = CString::new(name).ok()?;
    let handle = unsafe { dlopen(c_name.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        return None;
    }

    macro_rules! sym {
        ($sym:literal) => {{
            let ptr = unsafe { dlsym(handle, concat!($sym, "\0").as_ptr() as *const c_char) };
            if ptr.is_null() {
                log::warn!("pcsc_shim: {name} is missing symbol {}", $sym);
                return None;
            }
            unsafe { std::mem::transmute(ptr) }
        }};
    }

    Some(PcscBackend {
        establish_context: sym!("SCardEstablishContext"),
        release_context: sym!("SCardReleaseContext"),
        list_readers: sym!("SCardListReaders"),
        connect: sym!("SCardConnect"),
        disconnect: sym!("SCardDisconnect"),
        transmit: sym!("SCardTransmit"),
    })
}

fn backend() -> Option<&'static PcscBackend> {
    BACKEND
        .get_or_init(|| {
            let env_override = std::env::var("B25_PCSC_LIB").ok();
            // A library placed next to the executable takes priority over the
            // system library (dlopen with a bare name skips the exe dir, so
            // build absolute paths for it explicitly).
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            #[cfg(target_os = "macos")]
            const LOCAL_NAMES: &[&str] = &["libpcsclite.1.0.0.dylib", "libpcsclite.dylib"];
            #[cfg(target_os = "macos")]
            const SYSTEM_NAMES: &[&str] = &["libpcsclite.1.0.0.dylib", "libpcsclite.dylib"];
            #[cfg(not(target_os = "macos"))]
            const LOCAL_NAMES: &[&str] = &["libpcsckai.so", "libpcsclite.so.1", "libpcsclite.so"];
            #[cfg(not(target_os = "macos"))]
            const SYSTEM_NAMES: &[&str] = &["libpcsckai.so", "libpcsclite.so.1", "libpcsclite.so"];

            let exe_local: Vec<String> = exe_dir
                .iter()
                .flat_map(|d| {
                    LOCAL_NAMES
                        .iter()
                        .filter_map(|n| d.join(n).to_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .collect();

            let mut candidates: Vec<&str> = Vec::new();
            if let Some(ref path) = env_override {
                candidates.push(path);
            }
            candidates.extend(exe_local.iter().map(String::as_str));
            candidates.extend(SYSTEM_NAMES.iter().copied());
            #[cfg(target_os = "macos")]
            candidates.push("/System/Library/Frameworks/PCSC.framework/PCSC");

            for name in candidates {
                if let Some(b) = load_candidate(name) {
                    log::info!("pcsc_shim: using PC/SC backend {name}");
                    return Some(b);
                }
            }
            log::error!("pcsc_shim: no PC/SC backend found");
            None
        })
        .as_ref()
}

#[no_mangle]
pub unsafe extern "C" fn SCardEstablishContext(
    dw_scope: Dword,
    pv_reserved1: *const c_void,
    pv_reserved2: *const c_void,
    ph_context: *mut ScardContext,
) -> Long {
    if ph_context.is_null() {
        return SCARD_E_INVALID_PARAMETER;
    }
    let boncaslink_available = boncaslink::enabled() || boncaslink::has_registered_readers();
    if let Some(b) = backend() {
        let status = (b.establish_context)(dw_scope, pv_reserved1, pv_reserved2, ph_context);
        if status == 0 {
            let context = scard_handle_key(*ph_context);
            if !boncaslink::register_native_context(context) {
                let _ = (b.release_context)(*ph_context);
                return SCARD_E_INVALID_HANDLE;
            }
            #[cfg(target_os = "macos")]
            crate::px4_card::register_context(context);
            return 0;
        }
        let virtual_readers_available =
            virtual_readers_after_native_result(Some(status), boncaslink_available, || {
                !px4_readers().is_empty()
            });
        if !virtual_readers_available {
            return status;
        }
        let context = boncaslink::new_dummy_context();
        if context == 0 {
            return SCARD_E_NO_SERVICE;
        }
        #[cfg(target_os = "macos")]
        crate::px4_card::register_context(context);
        *ph_context = scard_handle_from_key(context);
        return 0;
    }
    let virtual_readers_available =
        virtual_readers_after_native_result(None, boncaslink_available, || {
            !px4_readers().is_empty()
        });
    if virtual_readers_available {
        let context = boncaslink::new_dummy_context();
        if context == 0 {
            return SCARD_E_NO_SERVICE;
        }
        #[cfg(target_os = "macos")]
        crate::px4_card::register_context(context);
        *ph_context = scard_handle_from_key(context);
        return 0;
    }
    SCARD_E_NO_SERVICE
}

pub(crate) fn release_native_context_with(
    h_context: ScardContext,
    release_backend: impl FnOnce(ScardContext) -> Long,
) -> Long {
    let context = scard_handle_key(h_context);
    let mut release = boncaslink::begin_native_context_release(context);
    if !release.was_registered() {
        return SCARD_E_INVALID_HANDLE;
    }

    #[cfg(target_os = "macos")]
    let status = {
        // PX4 has its own session registry. Retire and drain it with the
        // process-wide lifecycle mutex released, just like BonCasLink cards.
        release.release_lifecycle_lock();
        let px4_release = crate::px4_card::begin_context_release(context);
        release.reacquire_lifecycle_lock();
        let status = release_backend(h_context);
        let succeeded = status == 0;
        if succeeded {
            boncaslink::release_context_cards(context);
        }
        crate::px4_card::finish_context_release(px4_release, succeeded);
        boncaslink::finish_native_context_release(release, succeeded);
        status
    };

    #[cfg(not(target_os = "macos"))]
    let status = release_backend(h_context);
    #[cfg(not(target_os = "macos"))]
    let succeeded = status == 0;
    #[cfg(not(target_os = "macos"))]
    if succeeded {
        boncaslink::release_context_cards(context);
    }
    #[cfg(not(target_os = "macos"))]
    boncaslink::finish_native_context_release(release, succeeded);
    status
}

#[no_mangle]
pub unsafe extern "C" fn SCardReleaseContext(h_context: ScardContext) -> Long {
    let context = scard_handle_key(h_context);
    if boncaslink::is_dummy_context(context) {
        #[cfg(target_os = "macos")]
        crate::px4_card::release_context(context);
        boncaslink::release_dummy_context(context);
        return 0;
    }
    match backend() {
        Some(b) => release_native_context_with(h_context, |context| (b.release_context)(context)),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardListReaders(
    h_context: ScardContext,
    msz_groups: *const c_char,
    msz_readers: *mut c_char,
    pcch_readers: *mut Dword,
) -> Long {
    if pcch_readers.is_null() {
        return SCARD_E_INVALID_PARAMETER;
    }
    let px4 = px4_readers();
    let has_virtual_readers =
        boncaslink::enabled() || boncaslink::has_registered_readers() || !px4.is_empty();
    if !has_virtual_readers {
        return match backend() {
            Some(b) => (b.list_readers)(h_context, msz_groups, msz_readers, pcch_readers),
            None => SCARD_E_NO_SERVICE,
        };
    }
    let mut local = Vec::new();
    if !boncaslink::is_dummy_context(scard_handle_key(h_context)) {
        if let Some(b) = backend() {
            let mut required: Dword = 0;
            let status =
                (b.list_readers)(h_context, msz_groups, std::ptr::null_mut(), &mut required);
            if (status == 0 || status == SCARD_E_INSUFFICIENT_BUFFER)
                && required > 0
                && (required as usize) < (1 << 20)
            {
                let mut buffer = vec![0u8; required as usize];
                let mut actual = required;
                if (b.list_readers)(
                    h_context,
                    msz_groups,
                    buffer.as_mut_ptr() as *mut c_char,
                    &mut actual,
                ) == 0
                {
                    buffer.truncate((actual as usize).min(buffer.len()));
                    let mut rest = buffer.as_slice();
                    while let Some(end) = rest.iter().position(|byte| *byte == 0) {
                        if end == 0 {
                            break;
                        }
                        local.push(String::from_utf8_lossy(&rest[..end]).into_owned());
                        rest = &rest[end + 1..];
                    }
                }
            }
        }
    }
    let readers = merge_px4_readers(px4, boncaslink::prepend_reader(local));
    let readers = boncaslink::multi_string(&readers);
    let required = readers.len() as Dword;
    if msz_readers.is_null() {
        *pcch_readers = required;
        return 0;
    }
    if *pcch_readers < required {
        *pcch_readers = required;
        return SCARD_E_INSUFFICIENT_BUFFER;
    }
    std::ptr::copy_nonoverlapping(readers.as_ptr(), msz_readers as *mut u8, readers.len());
    *pcch_readers = required;
    0
}

#[no_mangle]
pub unsafe extern "C" fn SCardConnect(
    h_context: ScardContext,
    sz_reader: *const c_char,
    dw_share_mode: Dword,
    dw_preferred_protocols: Dword,
    ph_card: *mut ScardContext,
    pdw_active_protocol: *mut Dword,
) -> Long {
    if ph_card.is_null() || pdw_active_protocol.is_null() {
        return SCARD_E_INVALID_PARAMETER;
    }
    if !sz_reader.is_null() {
        let name = std::ffi::CStr::from_ptr(sz_reader).to_string_lossy();
        #[cfg(target_os = "macos")]
        if name.starts_with(crate::px4_card::READER_PREFIX) {
            return match crate::px4_card::connect_reader(scard_handle_key(h_context), &name) {
                Ok((card, protocol)) => {
                    *ph_card = scard_handle_from_key(card);
                    *pdw_active_protocol = protocol;
                    0
                }
                Err(crate::px4_card::Px4CardError::NoCard) => SCARD_E_NO_SMARTCARD,
                _ => SCARD_E_READER_UNAVAILABLE,
            };
        }
        if name == boncaslink::BONCASLINK_READER_NAME || name.starts_with("BonCasLink:") {
            return match boncaslink::connect_reader_in_context(scard_handle_key(h_context), &name) {
                Ok(card) => {
                    *ph_card = scard_handle_from_key(card);
                    *pdw_active_protocol = 2;
                    0
                }
                _ => SCARD_E_READER_UNAVAILABLE,
            };
        }
    }
    match backend() {
        Some(b) => {
            let context = scard_handle_key(h_context);
            let Some(_operation) = boncaslink::begin_native_context_operation(context) else {
                return SCARD_E_INVALID_HANDLE;
            };
            let mut backend_card: ScardContext = 0;
            let status = (b.connect)(
                h_context,
                sz_reader,
                dw_share_mode,
                dw_preferred_protocols,
                &mut backend_card,
                pdw_active_protocol,
            );
            if status == 0 {
                if let Some(alias) =
                    boncaslink::register_native_card_alias(context, scard_handle_key(backend_card))
                {
                    *ph_card = scard_handle_from_key(alias);
                } else {
                    let _ = (b.disconnect)(backend_card, 0);
                    return SCARD_E_READER_UNAVAILABLE;
                }
            }
            status
        }
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardDisconnect(h_card: ScardContext, dw_disposition: Dword) -> Long {
    let card = scard_handle_key(h_card);
    #[cfg(target_os = "macos")]
    if crate::px4_card::is_card(card) {
        return if crate::px4_card::disconnect(card).is_ok() {
            0
        } else {
            SCARD_E_READER_UNAVAILABLE
        };
    }
    if boncaslink::is_card(card) {
        return if boncaslink::disconnect(card) {
            0
        } else {
            SCARD_E_READER_UNAVAILABLE
        };
    }
    if boncaslink::is_native_alias_handle_range(card) {
        return boncaslink::with_native_card_disconnect(
            card,
            |backend_card| {
                backend().map_or(SCARD_E_NO_SERVICE, |b| {
                    (b.disconnect)(scard_handle_from_key(backend_card), dw_disposition)
                })
            },
            |status| *status == 0,
        )
        .unwrap_or(SCARD_E_INVALID_HANDLE);
    }
    if boncaslink::is_synthetic_card_handle_range(card) {
        return SCARD_E_INVALID_HANDLE;
    }
    match backend() {
        Some(b) => (b.disconnect)(h_card, dw_disposition),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardTransmit(
    h_card: ScardContext,
    pio_send_pci: *const c_void,
    pb_send_buffer: *const u8,
    cb_send_length: Dword,
    pio_recv_pci: *mut c_void,
    pb_recv_buffer: *mut u8,
    pcb_recv_length: *mut Dword,
) -> Long {
    let card = scard_handle_key(h_card);
    #[cfg(target_os = "macos")]
    if crate::px4_card::is_card(card) {
        if pio_send_pci.is_null()
            || pb_send_buffer.is_null()
            || pcb_recv_length.is_null()
            || (pb_recv_buffer.is_null() && *pcb_recv_length != 0)
            || cb_send_length == 0
            || cb_send_length as usize > crate::px4_card::MAX_DATA_SIZE
        {
            return SCARD_E_INVALID_PARAMETER;
        }
        let apdu = std::slice::from_raw_parts(pb_send_buffer, cb_send_length as usize);
        let response =
            match crate::px4_card::transmit_with_capacity(card, apdu, *pcb_recv_length as usize) {
                Ok(response) => response,
                Err(crate::px4_card::Px4TransmitError::InsufficientBuffer(required)) => {
                    *pcb_recv_length = required as Dword;
                    return SCARD_E_INSUFFICIENT_BUFFER;
                }
                Err(crate::px4_card::Px4TransmitError::InvalidParameter) => {
                    return SCARD_E_INVALID_PARAMETER;
                }
                Err(_) => return SCARD_E_NOT_TRANSACTED,
            };
        if !response.is_empty() {
            std::ptr::copy_nonoverlapping(response.as_ptr(), pb_recv_buffer, response.len());
        }
        *pcb_recv_length = response.len() as Dword;
        return 0;
    }
    if boncaslink::is_card(card) {
        if pio_send_pci.is_null()
            || pcb_recv_length.is_null()
            || (pb_recv_buffer.is_null() && *pcb_recv_length != 0)
            || cb_send_length > 255
            || cb_send_length == 0
            || pb_send_buffer.is_null()
        {
            return SCARD_E_INVALID_PARAMETER;
        }
        let apdu = std::slice::from_raw_parts(pb_send_buffer, cb_send_length as usize);
        let response =
            match boncaslink::transmit_with_capacity(card, apdu, *pcb_recv_length as usize) {
                Ok(response) => response,
                Err(boncaslink::CardTransmitError::InsufficientBuffer(required)) => {
                    *pcb_recv_length = required as Dword;
                    return SCARD_E_INSUFFICIENT_BUFFER;
                }
                Err(_) => return SCARD_E_NOT_TRANSACTED,
            };
        if !response.is_empty() {
            std::ptr::copy_nonoverlapping(response.as_ptr(), pb_recv_buffer, response.len());
        }
        *pcb_recv_length = response.len() as Dword;
        return 0;
    }
    if boncaslink::is_native_alias_handle_range(card) {
        return boncaslink::with_native_card_operation(card, |backend_card| {
            backend().map_or(SCARD_E_NO_SERVICE, |b| {
                (b.transmit)(
                    scard_handle_from_key(backend_card),
                    pio_send_pci,
                    pb_send_buffer,
                    cb_send_length,
                    pio_recv_pci,
                    pb_recv_buffer,
                    pcb_recv_length,
                )
            })
        })
        .unwrap_or(SCARD_E_INVALID_HANDLE);
    }
    if boncaslink::is_synthetic_card_handle_range(card) {
        return SCARD_E_INVALID_HANDLE;
    }
    match backend() {
        Some(b) => (b.transmit)(
            h_card,
            pio_send_pci,
            pb_send_buffer,
            cb_send_length,
            pio_recv_pci,
            pb_recv_buffer,
            pcb_recv_length,
        ),
        None => SCARD_E_NO_SERVICE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BONCASLINK_READER_NAME;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn network_specific_readers_come_after_the_px4_card() {
        let readers = merge_px4_readers(
            vec!["PX4 DriverHost: PIXELA XIT-SQR100 Smart Card Reader".to_owned()],
            vec![
                "Local PCSC Reader".to_owned(),
                "BonCasLink:acas.example:6900".to_owned(),
            ],
        );
        assert_eq!(
            readers,
            [
                "Local PCSC Reader",
                "PX4 DriverHost: PIXELA XIT-SQR100 Smart Card Reader",
                "BonCasLink:acas.example:6900",
            ]
        );
    }

    #[test]
    fn configured_boncaslink_reader_stays_ahead_of_px4_readers() {
        let readers = merge_px4_readers(
            vec!["PX4 DriverHost: PIXELA Smart Card Reader".to_owned()],
            vec![
                boncaslink::BONCASLINK_READER_NAME.to_owned(),
                "Local PCSC Reader".to_owned(),
            ],
        );

        assert_eq!(
            readers,
            [
                boncaslink::BONCASLINK_READER_NAME,
                "Local PCSC Reader",
                "PX4 DriverHost: PIXELA Smart Card Reader"
            ]
        );
    }

    #[test]
    fn successful_native_context_skips_the_px4_reader_probe() {
        let probes = std::sync::atomic::AtomicUsize::new(0);

        let available = virtual_readers_after_native_result(Some(0), false, || {
            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });

        assert!(!available);
        assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn mac_scardhandle_conversion_preserves_unsigned_32_bit_pattern() {
        let raw = 0xF123_4567u32 as i32;
        let key = mac_handle_key(raw);

        assert_eq!(key, 0xF123_4567usize);
        assert_eq!(mac_handle_from_key(key), raw);
        if usize::BITS > 32 {
            assert_ne!(key, raw as usize);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unix_scardhandle_conversion_uses_native_c_long_width() {
        let raw: ScardContext = -1;
        let key = scard_handle_key(raw);

        assert_eq!(
            std::mem::size_of::<ScardContext>(),
            std::mem::size_of::<usize>()
        );
        assert_eq!(key, usize::MAX);
        assert_eq!(scard_handle_from_key(key), raw);
    }

    #[test]
    fn native_release_failure_restores_context_and_alias_registration() {
        let _guard = boncaslink::test_lock();
        let context = 0x3200_0201usize;
        assert!(boncaslink::register_native_context(context));
        let alias = boncaslink::register_native_card_alias(context, 0x4abc_def0).unwrap();

        let status =
            release_native_context_with(scard_handle_from_key(context), |_| 0x8010_0003u32 as Long);

        assert_ne!(status, 0);
        assert_eq!(boncaslink::resolve_native_card(alias), Some(0x4abc_def0));
        let second_alias = boncaslink::register_native_card_alias(context, 0x4abc_def1).unwrap();
        assert_eq!(
            boncaslink::resolve_native_card(second_alias),
            Some(0x4abc_def1)
        );
        boncaslink::unregister_native_context(context);
    }

    #[test]
    fn native_release_success_cleans_old_alias_before_context_id_reuse() {
        let _guard = boncaslink::test_lock();
        let context = 0x3200_0202usize;
        assert!(boncaslink::register_native_context(context));
        let old_alias = boncaslink::register_native_card_alias(context, 0x4abc_def2).unwrap();

        assert_eq!(
            release_native_context_with(scard_handle_from_key(context), |_| 0),
            0
        );
        assert_eq!(boncaslink::resolve_native_card(old_alias), None);
        assert!(boncaslink::register_native_context(context));
        let new_alias = boncaslink::register_native_card_alias(context, 0x4abc_def3).unwrap();

        assert_eq!(boncaslink::resolve_native_card(old_alias), None);
        assert_eq!(
            boncaslink::resolve_native_card(new_alias),
            Some(0x4abc_def3)
        );
        boncaslink::unregister_native_context(context);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ffi_list_readers_keeps_boncaslink_before_px4_readers() {
        let _guard = boncaslink::test_lock();
        let px4_reader = "PX4 DriverHost: Synthetic Reader".to_owned();
        PX4_READER_OVERRIDE.with(|readers| {
            *readers.borrow_mut() = Some(vec![px4_reader.clone()]);
        });
        boncaslink::set_server(Some("127.0.0.1:6900")).unwrap();
        let context = boncaslink::new_dummy_context() as ScardContext;

        struct Cleanup(ScardContext);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    SCardReleaseContext(self.0);
                }
                PX4_READER_OVERRIDE.with(|readers| *readers.borrow_mut() = None);
                let _ = boncaslink::set_server(None);
            }
        }
        let _cleanup = Cleanup(context);

        unsafe {
            let mut length: Dword = 0;
            assert_eq!(
                SCardListReaders(context, std::ptr::null(), std::ptr::null_mut(), &mut length),
                0
            );
            let mut readers = vec![0; length as usize];
            assert_eq!(
                SCardListReaders(
                    context,
                    std::ptr::null(),
                    readers.as_mut_ptr() as *mut c_char,
                    &mut length,
                ),
                0
            );

            let names = readers
                .split(|byte| *byte == 0)
                .filter(|reader| !reader.is_empty())
                .collect::<Vec<_>>();
            assert_eq!(
                names.first().copied(),
                Some(BONCASLINK_READER_NAME.as_bytes())
            );
            assert!(names.contains(&px4_reader.as_bytes()));
        }
    }

    #[test]
    fn virtual_reader_round_trips_through_pcsc_shim() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut size = [0];
            stream.read_exact(&mut size).unwrap();
            let mut request = vec![0; size[0] as usize];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, [0x90, 0x30]);
            stream.write_all(&[2, 0x90, 0]).unwrap();
        });
        boncaslink::set_server(Some(&address.to_string())).unwrap();

        unsafe {
            let mut context: ScardContext = 0;
            assert_eq!(
                SCardEstablishContext(0, std::ptr::null(), std::ptr::null(), &mut context),
                0
            );
            let mut length: Dword = 0;
            assert_eq!(
                SCardListReaders(context, std::ptr::null(), std::ptr::null_mut(), &mut length),
                0
            );
            assert!(length >= BONCASLINK_READER_NAME.len() as Dword + 2);
            let mut short = [0u8; 1];
            let mut short_len = short.len() as Dword;
            assert_eq!(
                SCardListReaders(
                    context,
                    std::ptr::null(),
                    short.as_mut_ptr() as *mut c_char,
                    &mut short_len
                ),
                SCARD_E_INSUFFICIENT_BUFFER
            );
            assert_eq!(short_len, length);
            let mut readers = vec![0u8; length as usize];
            assert_eq!(
                SCardListReaders(
                    context,
                    std::ptr::null(),
                    readers.as_mut_ptr() as *mut c_char,
                    &mut length
                ),
                0
            );
            assert!(readers.starts_with(b"BonCasLink Client Card Reader\0"));

            let reader = CString::new(BONCASLINK_READER_NAME).unwrap();
            let mut card = 0;
            let mut protocol = 0;
            assert_eq!(
                SCardConnect(context, reader.as_ptr(), 2, 2, &mut card, &mut protocol),
                0
            );
            assert_eq!(protocol, 2);
            let mut send_pci = ScardIoRequest {
                dw_protocol: 2,
                cb_pci_length: PCI_LEN,
            };
            let command = [0x90, 0x30];
            let mut response = [0; 8];
            let mut response_len = response.len() as Dword;
            assert_eq!(
                SCardTransmit(
                    card,
                    &mut send_pci as *mut _ as *const c_void,
                    command.as_ptr(),
                    command.len() as Dword,
                    std::ptr::null_mut(),
                    response.as_mut_ptr(),
                    &mut response_len
                ),
                0
            );
            assert_eq!(&response[..response_len as usize], [0x90, 0]);
            assert_eq!(SCardDisconnect(card, 0), 0);
            assert_eq!(SCardReleaseContext(context), 0);
        }
        server.join().unwrap();
        boncaslink::set_server(None).unwrap();
    }

    #[test]
    fn boncaslink_connect_rejects_null_outputs_before_opening_a_card() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        boncaslink::set_server(Some(&address.to_string())).unwrap();
        let context = boncaslink::new_dummy_context() as ScardContext;
        let reader = CString::new(BONCASLINK_READER_NAME).unwrap();

        let status =
            unsafe { SCardConnect(context, reader.as_ptr(), 2, 2, std::ptr::null_mut(), &mut 0) };
        let accepted_connection = listener.accept().is_ok();

        unsafe { SCardReleaseContext(context) };
        boncaslink::set_server(None).unwrap();
        assert_eq!(status, SCARD_E_INVALID_PARAMETER);
        assert!(
            !accepted_connection,
            "invalid output pointers must not open BonCasLink"
        );
    }

    #[test]
    fn stale_synthetic_card_handle_does_not_fall_through_to_pcsc_backend() {
        let stale_card = 0x4fff_fffeusize;
        assert!(!boncaslink::is_card(stale_card));
        assert!(!boncaslink::is_native_card(stale_card));
        assert_eq!(
            unsafe { SCardDisconnect(stale_card as _, 0) },
            SCARD_E_INVALID_HANDLE
        );

        let send_pci = 0u8;
        let command = [0x90, 0x30];
        let mut response = [0u8; 8];
        let mut response_len = response.len() as Dword;
        assert_eq!(
            unsafe {
                SCardTransmit(
                    stale_card as _,
                    &send_pci as *const _ as *const c_void,
                    command.as_ptr(),
                    command.len() as Dword,
                    std::ptr::null_mut(),
                    response.as_mut_ptr(),
                    &mut response_len,
                )
            },
            SCARD_E_INVALID_HANDLE
        );
    }

    #[test]
    fn stale_native_alias_does_not_fall_through_to_pcsc_backend() {
        let stale_alias = 0x2000_4321usize;
        assert!(boncaslink::is_native_alias_handle_range(stale_alias));
        assert!(!boncaslink::is_native_card(stale_alias));
        assert_eq!(
            unsafe { SCardDisconnect(scard_handle_from_key(stale_alias), 0) },
            SCARD_E_INVALID_HANDLE
        );

        let send_pci = 0u8;
        let command = [0x90, 0x31];
        let mut response = [0u8; 8];
        let mut response_len = response.len() as Dword;
        assert_eq!(
            unsafe {
                SCardTransmit(
                    scard_handle_from_key(stale_alias),
                    &send_pci as *const _ as *const c_void,
                    command.as_ptr(),
                    command.len() as Dword,
                    std::ptr::null_mut(),
                    response.as_mut_ptr(),
                    &mut response_len,
                )
            },
            SCARD_E_INVALID_HANDLE
        );
    }

    #[test]
    fn releasing_dummy_context_closes_its_boncaslink_cards() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        boncaslink::set_server(Some(&address.to_string())).unwrap();
        let context = boncaslink::new_dummy_context() as ScardContext;
        let reader = CString::new(BONCASLINK_READER_NAME).unwrap();
        let (peer_closed_tx, peer_closed_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0u8; 1];
            peer_closed_tx
                .send(stream.read(&mut byte).is_ok_and(|length| length == 0))
                .unwrap();
        });

        let mut card = 0;
        let mut protocol = 0;
        assert_eq!(
            unsafe { SCardConnect(context, reader.as_ptr(), 2, 2, &mut card, &mut protocol) },
            0
        );
        assert!(boncaslink::is_card(card as usize));
        assert_eq!(unsafe { SCardReleaseContext(context) }, 0);

        assert!(peer_closed_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());
        assert!(!boncaslink::is_card(card as usize));
        server.join().unwrap();
        boncaslink::set_server(None).unwrap();
    }

    #[test]
    fn releasing_native_context_closes_its_boncaslink_cards() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        boncaslink::set_server(Some(&address.to_string())).unwrap();
        let context = 0x3100_0101usize;
        boncaslink::register_native_context(context);
        let peer_closed_rx = {
            let (peer_closed_tx, peer_closed_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut byte = [0u8; 1];
                peer_closed_tx
                    .send(stream.read(&mut byte).is_ok_and(|length| length == 0))
                    .unwrap();
            });
            peer_closed_rx
        };
        let card = boncaslink::connect_reader_in_context(context, BONCASLINK_READER_NAME).unwrap();

        let mut card_was_available_during_backend_release = false;
        let status = release_native_context_with(context as ScardContext, |_| {
            card_was_available_during_backend_release = boncaslink::is_card(card);
            0
        });

        assert_eq!(status, 0);
        assert!(card_was_available_during_backend_release);
        assert!(!boncaslink::is_card(card));
        assert!(peer_closed_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());
        boncaslink::set_server(None).unwrap();
    }

    #[test]
    fn failed_native_release_preserves_boncaslink_sessions_until_rollback() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).unwrap();
            let mut apdu = vec![0; length[0] as usize];
            stream.read_exact(&mut apdu).unwrap();
            stream.write_all(&[2, 0x90, 0]).unwrap();
        });
        boncaslink::set_server(Some(&address.to_string())).unwrap();
        let context = 0x3100_0102usize;
        assert!(boncaslink::register_native_context(context));
        let card = boncaslink::connect_reader_in_context(context, BONCASLINK_READER_NAME).unwrap();

        let mut session_existed_during_release = false;
        let mut connect_was_blocked = false;
        let mut transmit_was_blocked = false;
        let mut disconnect_was_blocked = false;
        let status = release_native_context_with(context as ScardContext, |_| {
            session_existed_during_release = boncaslink::is_card(card);
            connect_was_blocked =
                boncaslink::connect_reader_in_context(context, BONCASLINK_READER_NAME).is_err();
            transmit_was_blocked = boncaslink::transmit_with_capacity(card, &[0x90], usize::MAX)
                == Err(boncaslink::CardTransmitError::Unavailable);
            disconnect_was_blocked = !boncaslink::disconnect(card);
            0x8010_0003u32 as Long
        });

        assert_ne!(status, 0);
        assert!(session_existed_during_release);
        assert!(connect_was_blocked);
        assert!(transmit_was_blocked);
        assert!(disconnect_was_blocked);
        assert!(boncaslink::is_card(card));
        assert_eq!(
            boncaslink::transmit_with_capacity(card, &[0x90], usize::MAX).unwrap(),
            [0x90, 0]
        );
        assert!(boncaslink::disconnect(card));
        server.join().unwrap();
        boncaslink::unregister_native_context(context);
        boncaslink::set_server(None).unwrap();
    }

    #[test]
    fn boncaslink_different_apdu_discards_pending_response_and_runs() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut requests = Vec::new();
            loop {
                let mut size = [0];
                if stream.read_exact(&mut size).is_err() {
                    break;
                }
                let mut request = vec![0; size[0] as usize];
                stream.read_exact(&mut request).unwrap();
                requests.push(request);
                stream.write_all(&[2, 0x90, 0]).unwrap();
            }
            requests
        });
        boncaslink::set_server(Some(&address.to_string())).unwrap();

        let (
            context,
            card,
            send_pci,
            first_command,
            different_command,
            first_status,
            needed_len,
            different_status,
            retry_status,
            response,
            response_len,
        ) = unsafe {
            let mut context: ScardContext = 0;
            let establish_status =
                SCardEstablishContext(0, std::ptr::null(), std::ptr::null(), &mut context);
            let reader = CString::new(BONCASLINK_READER_NAME).unwrap();
            let mut card = 0;
            let mut protocol = 0;
            let connect_status =
                SCardConnect(context, reader.as_ptr(), 2, 2, &mut card, &mut protocol);
            let mut send_pci = ScardIoRequest {
                dw_protocol: 2,
                cb_pci_length: PCI_LEN,
            };
            let first_command = [0x90, 0x40];
            let different_command = [0x90, 0x41];
            let mut needed_len = 0 as Dword;
            let first_status = SCardTransmit(
                card,
                &mut send_pci as *mut _ as *const c_void,
                first_command.as_ptr(),
                first_command.len() as Dword,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut needed_len,
            );
            let mut response = [0u8; 8];
            let mut response_len = response.len() as Dword;
            let different_status = SCardTransmit(
                card,
                &mut send_pci as *mut _ as *const c_void,
                different_command.as_ptr(),
                different_command.len() as Dword,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_len,
            );
            response_len = response.len() as Dword;
            let retry_status = SCardTransmit(
                card,
                &mut send_pci as *mut _ as *const c_void,
                first_command.as_ptr(),
                first_command.len() as Dword,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_len,
            );
            assert_eq!(establish_status, 0);
            assert_eq!(connect_status, 0);
            let _ = SCardDisconnect(card, 0);
            let _ = SCardReleaseContext(context);
            (
                context,
                card,
                send_pci,
                first_command,
                different_command,
                first_status,
                needed_len,
                different_status,
                retry_status,
                response,
                response_len,
            )
        };
        let requests = server.join().unwrap();
        boncaslink::set_server(None).unwrap();

        assert_ne!(context, 0);
        assert_ne!(card, 0);
        assert_eq!(send_pci.dw_protocol, 2);
        assert_eq!(first_command, [0x90, 0x40]);
        assert_eq!(different_command, [0x90, 0x41]);
        assert_eq!(first_status, SCARD_E_INSUFFICIENT_BUFFER);
        assert_eq!(needed_len, 2);
        assert_eq!(different_status, 0);
        assert_eq!(retry_status, 0);
        assert_eq!(&response[..response_len as usize], [0x90, 0]);
        assert_eq!(
            requests,
            [
                first_command.to_vec(),
                different_command.to_vec(),
                first_command.to_vec()
            ]
        );
    }

    #[test]
    fn boncaslink_same_apdu_retry_reuses_cached_response() {
        let _guard = boncaslink::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut size = [0u8; 1];
            stream.read_exact(&mut size).unwrap();
            let mut request = vec![0u8; size[0] as usize];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(&[2, 0x90, 0]).unwrap();
            request
        });
        boncaslink::set_server(Some(&address.to_string())).unwrap();
        let context = boncaslink::new_dummy_context() as ScardContext;
        let reader = CString::new(BONCASLINK_READER_NAME).unwrap();
        let mut card = 0;
        let mut protocol = 0;
        assert_eq!(
            unsafe { SCardConnect(context, reader.as_ptr(), 2, 2, &mut card, &mut protocol) },
            0
        );
        let send_pci = 0u8;
        let command = [0x90, 0x43];
        let mut small_response = [0u8; 1];
        let mut small_length = small_response.len() as Dword;
        let first_status = unsafe {
            SCardTransmit(
                card,
                &send_pci as *const _ as *const c_void,
                command.as_ptr(),
                command.len() as Dword,
                std::ptr::null_mut(),
                small_response.as_mut_ptr(),
                &mut small_length,
            )
        };
        let mut response = [0u8; 8];
        let mut response_length = response.len() as Dword;
        let retry_status = unsafe {
            SCardTransmit(
                card,
                &send_pci as *const _ as *const c_void,
                command.as_ptr(),
                command.len() as Dword,
                std::ptr::null_mut(),
                response.as_mut_ptr(),
                &mut response_length,
            )
        };
        unsafe { SCardReleaseContext(context) };
        let request = server.join().unwrap();
        boncaslink::set_server(None).unwrap();

        assert_eq!(first_status, SCARD_E_INSUFFICIENT_BUFFER);
        assert_eq!(retry_status, 0);
        assert_eq!(&response[..response_length as usize], [0x90, 0]);
        assert_eq!(request, command);
    }
}
