//! Runtime PC/SC backend selection for Linux/Unix.
//!
//! libaribb25 (statically linked) references six SCard* functions. Instead of
//! putting `libpcsclite.so.1` into DT_NEEDED at link time, this module defines
//! those symbols itself and forwards them to a backend chosen at runtime via
//! dlopen, in this order:
//!
//! 1. the library named by the `B25_PCSC_LIB` environment variable (if set)
//! 2. `libpcsckai.so` (drop-in pcsclite ABI replacement)
//! 3. `libpcsclite.so.1` / `libpcsclite.so`
//!
//! If no backend can be loaded, every entry point returns SCARD_E_NO_SERVICE,
//! which libaribb25 reports as a card-reader initialization failure.
//!
//! These functions are called from C; they must never panic.

use std::ffi::{c_char, c_int, c_long, c_ulong, c_void, CString};
use std::sync::OnceLock;

// pcsclite error code (LP64 value, matches pcsclite.h on Linux):
// "The Smart card resource manager is not running."
const SCARD_E_NO_SERVICE: c_long = 0x8010_001D;

const RTLD_NOW: c_int = 2;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

// Function pointer types mirror pcsclite's LP64 ABI:
// LONG = c_long, DWORD = c_ulong, SCARDCONTEXT/SCARDHANDLE = c_long.
type EstablishFn =
    unsafe extern "C" fn(c_ulong, *const c_void, *const c_void, *mut c_long) -> c_long;
type ReleaseFn = unsafe extern "C" fn(c_long) -> c_long;
type ListReadersFn =
    unsafe extern "C" fn(c_long, *const c_char, *mut c_char, *mut c_ulong) -> c_long;
type ConnectFn = unsafe extern "C" fn(
    c_long,
    *const c_char,
    c_ulong,
    c_ulong,
    *mut c_long,
    *mut c_ulong,
) -> c_long;
type DisconnectFn = unsafe extern "C" fn(c_long, c_ulong) -> c_long;
type TransmitFn = unsafe extern "C" fn(
    c_long,
    *const c_void,
    *const u8,
    c_ulong,
    *mut c_void,
    *mut u8,
    *mut c_ulong,
) -> c_long;

/// pcsclite's SCARD_IO_REQUEST. libaribb25 references the pcsclite-exported
/// constants g_rgSCard*Pci (via the SCARD_PCI_* macros) as data symbols, which
/// cannot be forwarded through dlsym. Their contents are fixed protocol
/// descriptors, so we export identical copies ourselves; backends only read
/// the pointed-to values.
#[repr(C)]
pub struct ScardIoRequest {
    dw_protocol: c_ulong,
    cb_pci_length: c_ulong,
}

const PCI_LEN: c_ulong = std::mem::size_of::<ScardIoRequest>() as c_ulong;

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
            // A .so placed next to the executable takes priority over the
            // system library (dlopen with a bare name skips the exe dir, so
            // build absolute paths for it explicitly).
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            let exe_local: Vec<String> = exe_dir
                .iter()
                .flat_map(|d| {
                    ["libpcsckai.so", "libpcsclite.so.1", "libpcsclite.so"]
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
            candidates.extend(["libpcsckai.so", "libpcsclite.so.1", "libpcsclite.so"]);

            for name in candidates {
                if let Some(b) = load_candidate(name) {
                    log::info!("pcsc_shim: using PC/SC backend {name}");
                    return Some(b);
                }
            }
            log::error!(
                "pcsc_shim: no PC/SC backend found (tried B25_PCSC_LIB, libpcsckai.so, libpcsclite.so.1)"
            );
            None
        })
        .as_ref()
}

#[no_mangle]
pub unsafe extern "C" fn SCardEstablishContext(
    dw_scope: c_ulong,
    pv_reserved1: *const c_void,
    pv_reserved2: *const c_void,
    ph_context: *mut c_long,
) -> c_long {
    match backend() {
        Some(b) => (b.establish_context)(dw_scope, pv_reserved1, pv_reserved2, ph_context),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardReleaseContext(h_context: c_long) -> c_long {
    match backend() {
        Some(b) => (b.release_context)(h_context),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardListReaders(
    h_context: c_long,
    msz_groups: *const c_char,
    msz_readers: *mut c_char,
    pcch_readers: *mut c_ulong,
) -> c_long {
    match backend() {
        Some(b) => (b.list_readers)(h_context, msz_groups, msz_readers, pcch_readers),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardConnect(
    h_context: c_long,
    sz_reader: *const c_char,
    dw_share_mode: c_ulong,
    dw_preferred_protocols: c_ulong,
    ph_card: *mut c_long,
    pdw_active_protocol: *mut c_ulong,
) -> c_long {
    match backend() {
        Some(b) => (b.connect)(
            h_context,
            sz_reader,
            dw_share_mode,
            dw_preferred_protocols,
            ph_card,
            pdw_active_protocol,
        ),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardDisconnect(h_card: c_long, dw_disposition: c_ulong) -> c_long {
    match backend() {
        Some(b) => (b.disconnect)(h_card, dw_disposition),
        None => SCARD_E_NO_SERVICE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn SCardTransmit(
    h_card: c_long,
    pio_send_pci: *const c_void,
    pb_send_buffer: *const u8,
    cb_send_length: c_ulong,
    pio_recv_pci: *mut c_void,
    pb_recv_buffer: *mut u8,
    pcb_recv_length: *mut c_ulong,
) -> c_long {
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
