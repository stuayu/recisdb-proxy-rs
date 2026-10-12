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
    if let Some(b) = backend() {
        let status = (b.establish_context)(dw_scope, pv_reserved1, pv_reserved2, ph_context);
        if status == 0 || (!boncaslink::enabled() && !boncaslink::has_registered_readers()) {
            if status == 0 && !ph_context.is_null() {
                boncaslink::register_native_context(*ph_context as usize);
            }
            return status;
        }
    }
    if (boncaslink::enabled() || boncaslink::has_registered_readers()) && !ph_context.is_null() {
        let context = boncaslink::new_dummy_context();
        if context == 0 {
            return SCARD_E_NO_SERVICE;
        }
        *ph_context = context as ScardContext;
        return 0;
    }
    SCARD_E_NO_SERVICE
}

#[no_mangle]
pub unsafe extern "C" fn SCardReleaseContext(h_context: ScardContext) -> Long {
    if boncaslink::is_dummy_context(h_context as usize) {
        boncaslink::release_dummy_context(h_context as usize);
        return 0;
    }
    match backend() {
        Some(b) => {
            let status = (b.release_context)(h_context);
            if status == 0 {
                boncaslink::unregister_native_context(h_context as usize);
            }
            status
        }
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
    if !boncaslink::enabled() && !boncaslink::has_registered_readers() {
        return match backend() {
            Some(b) => (b.list_readers)(h_context, msz_groups, msz_readers, pcch_readers),
            None => SCARD_E_NO_SERVICE,
        };
    }
    if pcch_readers.is_null() {
        return SCARD_E_INVALID_PARAMETER;
    }
    let mut local = Vec::new();
    if !boncaslink::is_dummy_context(h_context as usize) {
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
    let readers = boncaslink::multi_string(&boncaslink::prepend_reader(local));
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
    if !sz_reader.is_null() {
        let name = std::ffi::CStr::from_ptr(sz_reader).to_string_lossy();
        if name == boncaslink::BONCASLINK_READER_NAME || name.starts_with("BonCasLink:") {
            return match boncaslink::connect_reader(&name) {
                Ok(card) if !ph_card.is_null() && !pdw_active_protocol.is_null() => {
                    *ph_card = card as ScardContext;
                    *pdw_active_protocol = 2;
                    0
                }
                _ => SCARD_E_READER_UNAVAILABLE,
            };
        }
    }
    match backend() {
        Some(b) => {
            let status = (b.connect)(
                h_context,
                sz_reader,
                dw_share_mode,
                dw_preferred_protocols,
                ph_card,
                pdw_active_protocol,
            );
            if status == 0 && !ph_card.is_null() {
                boncaslink::register_native_card(*ph_card as usize);
            }
            status
        }
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardDisconnect(h_card: ScardContext, dw_disposition: Dword) -> Long {
    let card = h_card as usize;
    if boncaslink::is_card(card) {
        return if boncaslink::disconnect(card) {
            0
        } else {
            SCARD_E_READER_UNAVAILABLE
        };
    }
    match backend() {
        Some(b) => {
            let status = (b.disconnect)(h_card, dw_disposition);
            if status == 0 {
                boncaslink::unregister_native_card(card);
            }
            status
        }
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
    let card = h_card as usize;
    if boncaslink::is_card(card) {
        if pio_send_pci.is_null()
            || pb_recv_buffer.is_null()
            || pcb_recv_length.is_null()
            || cb_send_length > 255
            || cb_send_length == 0
            || pb_send_buffer.is_null()
        {
            return SCARD_E_INVALID_PARAMETER;
        }
        let apdu = std::slice::from_raw_parts(pb_send_buffer, cb_send_length as usize);
        let response = match boncaslink::transmit(card, apdu) {
            Ok(response) => response,
            Err(()) => return SCARD_E_NOT_TRANSACTED,
        };
        if (*pcb_recv_length as usize) < response.len() {
            *pcb_recv_length = response.len() as Dword;
            return SCARD_E_INSUFFICIENT_BUFFER;
        }
        std::ptr::copy_nonoverlapping(response.as_ptr(), pb_recv_buffer, response.len());
        *pcb_recv_length = response.len() as Dword;
        return 0;
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
}
