//! Windows BonDriver wrapper implementation.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::io;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use cpp_utils::{DynamicCast, MutPtr, Ptr};
use log::{debug, info, error};

// Include generated bindings
include!(concat!(env!("OUT_DIR"), "/BonDriver_binding.rs"));

// FFI declarations for C++ wrapper functions

mod ib1 {
    use super::{IBonDriver, BOOL, BYTE, DWORD};

    extern "C" {
        pub fn C_OpenTuner(b: *mut IBonDriver) -> BOOL;
        pub fn C_CloseTuner(b: *mut IBonDriver);
        pub fn C_SetChannel(b: *mut IBonDriver, bCh: BYTE) -> BOOL;
        pub fn C_GetSignalLevel(b: *mut IBonDriver) -> f32;

        pub fn C_WaitTsStream(b: *mut IBonDriver, dwTimeOut: DWORD) -> DWORD;

        // ★ 修正：this が必要
        pub fn C_GetReadyCount(b: *mut IBonDriver) -> DWORD;

        // ★ 1つ目：BYTE* dst 版（コピーしてもらう）
        pub fn C_GetTsStream(
            b: *mut IBonDriver,
            pDst: *mut BYTE,
            pdwSize: *mut DWORD,
            pdwRemain: *mut DWORD,
        ) -> BOOL;

        // ★ 追加：BYTE** ppDst 版（ゼロコピーの可能性がある）
        pub fn C_GetTsStream2(
            b: *mut IBonDriver,
            ppDst: *mut *mut BYTE,
            pdwSize: *mut DWORD,
            pdwRemain: *mut DWORD,
        ) -> BOOL;

        pub fn C_PurgeTsStream(b: *mut IBonDriver);
        pub fn C_Release(b: *mut IBonDriver);
        pub fn CreateBonDriver() -> *mut IBonDriver;
    }
}


mod ib2 {
    use super::{IBonDriver2, BOOL, DWORD, LPCTSTR};

    extern "C" {
        pub fn C_EnumTuningSpace(b: *mut IBonDriver2, dwSpace: DWORD) -> LPCTSTR;
        pub fn C_EnumChannelName2(b: *mut IBonDriver2, dwSpace: DWORD, dwChannel: DWORD) -> LPCTSTR;
        pub fn C_SetChannel2(b: *mut IBonDriver2, dwSpace: DWORD, dwChannel: DWORD) -> BOOL;
    }
}

mod ib3 {
    use super::{IBonDriver3, BOOL};

    extern "C" {
        pub fn C_SetLnbPower(b: *mut IBonDriver3, bEnable: BOOL) -> BOOL;
    }
}

mod ib_utils {
    use super::{IBonDriver, IBonDriver2, IBonDriver3};

    extern "C" {
        pub fn interface_check_2(i: *mut IBonDriver) -> *mut IBonDriver2;
        pub fn interface_check_3(i: *mut IBonDriver2) -> *mut IBonDriver3;
        pub fn interface_check_2_const(i: *const IBonDriver) -> *const IBonDriver2;
        pub fn interface_check_3_const(i: *const IBonDriver2) -> *const IBonDriver3;
    }

    pub fn from_wide_ptr(ptr: *const u16) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        unsafe {
            let len = (0..std::isize::MAX)
                .position(|i| *ptr.offset(i) == 0)
                .unwrap();
            if len == 0 {
                return None;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            String::from_utf16(slice).ok()
        }
    }
}

impl DynamicCast<IBonDriver2> for IBonDriver {
    unsafe fn dynamic_cast(ptr: Ptr<Self>) -> Ptr<IBonDriver2> {
        Ptr::from_raw(ib_utils::interface_check_2_const(ptr.as_raw_ptr()))
    }

    unsafe fn dynamic_cast_mut(ptr: MutPtr<Self>) -> MutPtr<IBonDriver2> {
        MutPtr::from_raw(ib_utils::interface_check_2(ptr.as_mut_raw_ptr()))
    }
}

impl DynamicCast<IBonDriver3> for IBonDriver2 {
    unsafe fn dynamic_cast(ptr: Ptr<Self>) -> Ptr<IBonDriver3> {
        Ptr::from_raw(ib_utils::interface_check_3_const(ptr.as_raw_ptr()))
    }

    unsafe fn dynamic_cast_mut(ptr: MutPtr<Self>) -> MutPtr<IBonDriver3> {
        MutPtr::from_raw(ib_utils::interface_check_3(ptr.as_mut_raw_ptr()))
    }
}

/// Internal BonDriver interface wrapper.
struct IBon {
    version: u8,
    ibon1: NonNull<IBonDriver>,
    ibon2: Option<NonNull<IBonDriver2>>,
    ibon3: Option<NonNull<IBonDriver3>>,
}

impl Drop for IBon {
    fn drop(&mut self) {
        self.ibon3 = None;
        self.ibon2 = None;
        unsafe {
            ib1::C_Release(self.ibon1.as_ptr());
        }
    }
}

impl IBon {
    fn open_tuner(&self) -> Result<(), io::Error> {
        unsafe {
            if ib1::C_OpenTuner(self.ibon1.as_ptr()) != 0 {
                info!("[BonDriver] OpenTuner succeeded");
                Ok(())
            } else {
                let msg = format!("OpenTuner failed - tuner may be in use, not present, or hardware error");
                error!("[BonDriver] {}", msg);
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg))
            }
        }
    }

    fn close_tuner(&self) {
        unsafe {
            ib1::C_CloseTuner(self.ibon1.as_ptr());
        }
    }

    fn set_channel(&self, ch: u8) -> Result<(), io::Error> {
        unsafe {
            if ib1::C_SetChannel(self.ibon1.as_ptr(), ch) != 0 {
                Ok(())
            } else {
                let msg = format!("SetChannel failed for channel={} - channel may not exist or tuner not ready", ch);
                error!("[BonDriver] {}", msg);
                Err(io::Error::new(io::ErrorKind::AddrNotAvailable, msg))
            }
        }
    }

    fn set_channel_by_space(&self, space: u32, ch: u32) -> Result<(), io::Error> {
        let iface = self.ibon2.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "IBonDriver2 not supported by this driver")
        })?;
        unsafe {
            if ib2::C_SetChannel2(iface.as_ptr(), space, ch) != 0 {
                Ok(())
            } else {
                let msg = format!("SetChannel2 failed for space={}, channel={} - channel may not exist or tuner not ready", space, ch);
                error!("[BonDriver] {}", msg);
                Err(io::Error::new(io::ErrorKind::AddrNotAvailable, msg))
            }
        }
    }

    fn get_signal_level(&self) -> f32 {
        unsafe { ib1::C_GetSignalLevel(self.ibon1.as_ptr()) }
    }

    fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        unsafe { ib1::C_WaitTsStream(self.ibon1.as_ptr(), timeout_ms) != 0 }
    }


    pub(crate) fn GetTsStream<'a>(
        &self,
        buf: &'a mut [u8],
    ) -> Result<(&'a [u8], usize), io::Error> {
        // ★ 重要：毎回「このバッファに最大いくつ書けるか」を入れて渡す
        let mut size: u32 = buf.len().min(u32::MAX as usize) as u32;
        let mut remaining: u32 = 0;

        let iface = self.ibon1.as_ptr();
        unsafe {
            let ok = ib1::C_GetTsStream(
                iface,
                buf.as_mut_ptr(),
                &mut size as *mut u32,
                &mut remaining as *mut u32,
            ) != 0;

            if !ok {
                // BonDriver によっては「データ無し」で FALSE を返すことがあるので EOF は不適切
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "GetTsStream no data"));
            }

            // 念のためガード（FFIが壊れていると size が異常値になることがある）
            let size_usize = size as usize;
            if size_usize > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "GetTsStream returned size > buffer"));
            }

            Ok((&buf[..size_usize], remaining as usize))
        }
    }


    fn purge_ts_stream(&self) {
        unsafe {
            ib1::C_PurgeTsStream(self.ibon1.as_ptr());
        }
    }

    fn enum_tuning_space(&self, space: u32) -> Option<String> {
        let iface = self.ibon2?;
        unsafe {
            let ptr = ib2::C_EnumTuningSpace(iface.as_ptr(), space);
            ib_utils::from_wide_ptr(ptr)
        }
    }

    fn enum_channel_name(&self, space: u32, ch: u32) -> Option<String> {
        let iface = self.ibon2?;
        unsafe {
            let ptr = ib2::C_EnumChannelName2(iface.as_ptr(), space, ch);
            ib_utils::from_wide_ptr(ptr)
        }
    }
}

/// High-level BonDriver tuner wrapper.
pub struct BonDriverTuner {
    _dll: ManuallyDrop<BonDriver>,
    ibon: ManuallyDrop<IBon>,
}

impl Drop for BonDriverTuner {
    fn drop(&mut self) {
        unsafe {
            self.ibon.close_tuner();
            ManuallyDrop::drop(&mut self.ibon);
            ManuallyDrop::drop(&mut self._dll);
        }
    }
}

impl BonDriverTuner {
    /// Create a new BonDriver tuner from a DLL path.
    pub fn new(path: &str) -> Result<Self, io::Error> {
        // Verify the path exists first
        let path_str = path.to_string();
        if !std::path::Path::new(path).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("BonDriver file not found: {}", path)
            ));
        }

        let path_canonical = std::fs::canonicalize(path)
            .map_err(|e| io::Error::new(
                io::ErrorKind::NotFound,
                format!("Cannot access BonDriver path {}: {}", path, e)
            ))?;

        info!("[BonDriver] Loading {:?}...", path_canonical);

        let dll = unsafe {
            BonDriver::new(&path_canonical)
                .map_err(|e| {
                    let msg = format!("Failed to load BonDriver DLL {}: {}", path, e);
                    error!("[BonDriver] {}", msg);
                    io::Error::new(io::ErrorKind::NotFound, msg)
                })?
        };

        let ibon = {
            let ptr = unsafe { dll.CreateBonDriver() };
            let ibon1 = NonNull::new(ptr)
                .ok_or_else(|| {
                    let msg = format!("BonDriver.CreateBonDriver() returned null for {}", path);
                    error!("[BonDriver] {}", msg);
                    io::Error::new(io::ErrorKind::Other, msg)
                })?;

            let (ibon2, ibon3) = unsafe {
                let ptr: MutPtr<IBonDriver> = MutPtr::from_raw(ibon1.as_ptr());
                let ibon2 = ptr.dynamic_cast_mut();
                let ibon3 = ibon2.dynamic_cast_mut();
                (
                    NonNull::new(ibon2.as_mut_raw_ptr()),
                    NonNull::new(ibon3.as_mut_raw_ptr()),
                )
            };

            let version = match (ibon2, ibon3) {
                (None, None) => 1,
                (Some(_), None) => 2,
                (Some(_), Some(_)) => 3,
                _ => 0,
            };

            info!("[BonDriver] Interface version: {} for {}", version, path);

            IBon {
                version,
                ibon1,
                ibon2,
                ibon3,
            }
        };

        // Open tuner
        ibon.open_tuner()?;
        info!("[BonDriver] Tuner opened successfully");

        Ok(Self {
            _dll: ManuallyDrop::new(dll),
            ibon: ManuallyDrop::new(ibon),
        })
    }

    /// Set the channel by space and channel number.
    pub fn set_channel(&self, space: u32, channel: u32) -> Result<(), io::Error> {
        debug!("[BonDriver] SetChannel: space={}, channel={}", space, channel);
        self.ibon.set_channel_by_space(space, channel)
    }

    /// Get the current signal level.
    pub fn get_signal_level(&self) -> f32 {
        self.ibon.get_signal_level()
    }

    /// Wait for TS stream data to become available.
    /// Returns true if data is available, false on timeout.
    pub fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        self.ibon.wait_ts_stream(timeout_ms)
    }

    /// Get TS stream data.
    pub fn get_ts_stream(&self, buf: &mut [u8]) -> Result<(usize, usize), io::Error> {
        let ibon: &IBon = &*self.ibon; // ManuallyDrop<IBon> -> &IBon
        let (slice, remaining) = ibon.GetTsStream(buf)?; // PascalCase
        Ok((slice.len(), remaining))
    }

    /// Purge the TS stream buffer.
    pub fn purge_ts_stream(&self) {
        self.ibon.purge_ts_stream()
    }

    /// Enumerate tuning space name.
    pub fn enum_tuning_space(&self, space: u32) -> Option<String> {
        self.ibon.enum_tuning_space(space)
    }

    /// Enumerate channel name.
    pub fn enum_channel_name(&self, space: u32, channel: u32) -> Option<String> {
        self.ibon.enum_channel_name(space, channel)
    }

    /// Get the BonDriver version.
    pub fn version(&self) -> u8 {
        self.ibon.version
    }
}
