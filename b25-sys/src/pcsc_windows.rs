//! Windows PC/SC bridge. The native library calls remain in winscard.dll;
//! libaribb25's ANSI-independent UNICODE calls are patched to these exports.

use crate::boncaslink;
use std::ffi::c_void;

type Dword = u32;
type Long = i32;
type Context = usize;
const SUCCESS: Long = 0;
const NO_SERVICE: Long = 0x8010_001D_u32 as i32;
const READER_UNAVAILABLE: Long = 0x8010_0017_u32 as i32;
const INSUFFICIENT_BUFFER: Long = 0x8010_0008_u32 as i32;
const INVALID_PARAMETER: Long = 0x8010_0004_u32 as i32;
const NOT_TRANSACTED: Long = 0x8010_0016_u32 as i32;

#[link(name = "winscard")]
extern "system" {
    #[link_name = "SCardEstablishContext"]
    fn native_establish(
        scope: Dword,
        r1: *const c_void,
        r2: *const c_void,
        context: *mut Context,
    ) -> Long;
    #[link_name = "SCardReleaseContext"]
    fn native_release(context: Context) -> Long;
    #[link_name = "SCardListReadersW"]
    fn native_list_w(
        context: Context,
        groups: *const u16,
        readers: *mut u16,
        len: *mut Dword,
    ) -> Long;
    #[link_name = "SCardListReadersA"]
    fn native_list_a(
        context: Context,
        groups: *const u8,
        readers: *mut u8,
        len: *mut Dword,
    ) -> Long;
    #[link_name = "SCardConnectW"]
    fn native_connect_w(
        context: Context,
        reader: *const u16,
        share: Dword,
        protocols: Dword,
        card: *mut Context,
        active: *mut Dword,
    ) -> Long;
    #[link_name = "SCardDisconnect"]
    fn native_disconnect(card: Context, disposition: Dword) -> Long;
    #[link_name = "SCardTransmit"]
    fn native_transmit(
        card: Context,
        send_pci: *const c_void,
        send: *const u8,
        send_len: Dword,
        recv_pci: *mut c_void,
        recv: *mut u8,
        recv_len: *mut Dword,
    ) -> Long;
}

fn parse_multi(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let Some(end) = bytes[offset..].iter().position(|value| *value == 0) else {
            break;
        };
        if end == 0 {
            break;
        }
        result.push(bytes[offset..offset + end].to_vec());
        offset += end + 1;
    }
    result
}

fn parse_multi_wide(units: &[u16]) -> Vec<Vec<u16>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < units.len() {
        let Some(end) = units[offset..].iter().position(|value| *value == 0) else {
            break;
        };
        if end == 0 {
            break;
        }
        result.push(units[offset..offset + end].to_vec());
        offset += end + 1;
    }
    result
}

fn reader_name_matches_w(reader: *const u16) -> bool {
    if reader.is_null() {
        return false;
    }
    let mut length = 0usize;
    unsafe {
        while length < 32768 && *reader.add(length) != 0 {
            length += 1;
        }
        if length >= 32768 {
            return false;
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(reader, length));
        name.starts_with("BonCasLink:") || name == boncaslink::BONCASLINK_READER_NAME
    }
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardEstablishContext(
    scope: Dword,
    r1: *const c_void,
    r2: *const c_void,
    context: *mut Context,
) -> Long {
    if context.is_null() {
        return INVALID_PARAMETER;
    }
    let status = native_establish(scope, r1, r2, context);
    if status == SUCCESS || (!boncaslink::enabled() && !boncaslink::has_registered_readers()) {
        if status == SUCCESS {
            boncaslink::register_native_context(*context);
        }
        return status;
    }
    let dummy = boncaslink::new_dummy_context();
    if dummy == 0 {
        return NO_SERVICE;
    }
    *context = dummy;
    SUCCESS
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardReleaseContext(context: Context) -> Long {
    if boncaslink::release_dummy_context(context) {
        SUCCESS
    } else {
        let status = native_release(context);
        if status == SUCCESS {
            boncaslink::unregister_native_context(context);
        }
        status
    }
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardListReaders(
    context: Context,
    groups: *const u16,
    readers: *mut u16,
    len: *mut Dword,
) -> Long {
    if !boncaslink::enabled() && !boncaslink::has_registered_readers() {
        return native_list_w(context, groups, readers, len);
    }
    if len.is_null() {
        return INVALID_PARAMETER;
    }
    let mut local = Vec::new();
    let mut needed = 0;
    let mut status = if boncaslink::is_dummy_context(context) {
        NO_SERVICE
    } else {
        native_list_w(context, groups, std::ptr::null_mut(), &mut needed)
    };
    if (status == SUCCESS || status == INSUFFICIENT_BUFFER) && needed > 0 && needed < 1_000_000 {
        let mut buffer = vec![0u16; needed as usize];
        status = native_list_w(context, groups, buffer.as_mut_ptr(), &mut needed);
        if status == SUCCESS {
            buffer.truncate((needed as usize).min(buffer.len()));
            local = parse_multi_wide(&buffer);
        }
    }
    let _ = status; // Native readers are optional when the virtual reader is enabled.
    let names = boncaslink::prepend_reader(
        local
            .into_iter()
            .map(|entry| String::from_utf16_lossy(&entry))
            .collect(),
    );
    let mut output = Vec::new();
    for name in names {
        output.extend(name.encode_utf16());
        output.push(0);
    }
    output.push(0);
    write_output(&output, readers, len)
}

unsafe fn write_output<T: Copy>(values: &[T], output: *mut T, length: *mut Dword) -> Long {
    if length.is_null() {
        return INVALID_PARAMETER;
    }
    let required = values.len() as Dword;
    if output.is_null() {
        *length = required;
        return SUCCESS;
    }
    if *length < required {
        *length = required;
        return INSUFFICIENT_BUFFER;
    }
    std::ptr::copy_nonoverlapping(values.as_ptr(), output, values.len());
    *length = required;
    SUCCESS
}

/// ANSI variant used by b25-sys' public reader-listing helper.
#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardListReadersA(
    context: Context,
    groups: *const u8,
    readers: *mut u8,
    len: *mut Dword,
) -> Long {
    if !boncaslink::enabled() && !boncaslink::has_registered_readers() {
        return native_list_a(context, groups, readers, len);
    }
    if len.is_null() {
        return INVALID_PARAMETER;
    }
    let mut local = Vec::new();
    let mut needed = 0;
    let status = if boncaslink::is_dummy_context(context) {
        NO_SERVICE
    } else {
        native_list_a(context, groups, std::ptr::null_mut(), &mut needed)
    };
    if (status == SUCCESS || status == INSUFFICIENT_BUFFER) && needed > 0 && needed < 1_000_000 {
        let mut buffer = vec![0u8; needed as usize];
        if native_list_a(context, groups, buffer.as_mut_ptr(), &mut needed) == SUCCESS {
            buffer.truncate((needed as usize).min(buffer.len()));
            local = parse_multi(&buffer);
        }
    }
    local.retain(|entry| entry != boncaslink::BONCASLINK_READER_NAME.as_bytes());
    let names = boncaslink::prepend_reader(
        local
            .iter()
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect(),
    );
    write_output(&boncaslink::multi_string(&names), readers, len)
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardConnect(
    context: Context,
    reader: *const u16,
    share: Dword,
    protocols: Dword,
    card: *mut Context,
    active: *mut Dword,
) -> Long {
    if reader_name_matches_w(reader) {
        if card.is_null() || active.is_null() {
            return INVALID_PARAMETER;
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(
            reader,
            (0..32768).take_while(|i| *reader.add(*i) != 0).count(),
        ));
        return match boncaslink::connect_reader(&name) {
            Ok(id) => {
                *card = id;
                *active = 2;
                SUCCESS
            }
            Err(()) => READER_UNAVAILABLE,
        };
    }
    let status = native_connect_w(context, reader, share, protocols, card, active);
    if status == SUCCESS && !card.is_null() {
        boncaslink::register_native_card(*card);
    }
    status
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardDisconnect(card: Context, disposition: Dword) -> Long {
    if boncaslink::is_card(card) {
        if boncaslink::disconnect(card) {
            SUCCESS
        } else {
            READER_UNAVAILABLE
        }
    } else {
        let status = native_disconnect(card, disposition);
        if status == SUCCESS {
            boncaslink::unregister_native_card(card);
        }
        status
    }
}

#[no_mangle]
pub unsafe extern "system" fn b25shim_SCardTransmit(
    card: Context,
    send_pci: *const c_void,
    send: *const u8,
    send_len: Dword,
    recv_pci: *mut c_void,
    recv: *mut u8,
    recv_len: *mut Dword,
) -> Long {
    if !boncaslink::is_card(card) {
        return native_transmit(card, send_pci, send, send_len, recv_pci, recv, recv_len);
    }
    if send_pci.is_null()
        || send.is_null()
        || recv.is_null()
        || recv_len.is_null()
        || send_len == 0
        || send_len > 255
    {
        return INVALID_PARAMETER;
    }
    let response =
        match boncaslink::transmit(card, std::slice::from_raw_parts(send, send_len as usize)) {
            Ok(value) => value,
            Err(()) => return NOT_TRANSACTED,
        };
    if (*recv_len as usize) < response.len() {
        *recv_len = response.len() as Dword;
        return INSUFFICIENT_BUFFER;
    }
    std::ptr::copy_nonoverlapping(response.as_ptr(), recv, response.len());
    *recv_len = response.len() as Dword;
    SUCCESS
}

// Kept referenced to ensure the C ABI's handle width is evaluated here.
const _: () = assert!(std::mem::size_of::<Context>() == std::mem::size_of::<usize>());
