//! BonDriver interface definitions.
//!
//! This module defines the vtable structure for IBonDriver/IBonDriver2/IBonDriver3.
//!
//! IMPORTANT: MSVC RTTI layout requires a pointer at vtable[-1] that points to
//! the Complete Object Locator (COL). To support dynamic_cast in TVTest, we
//! include this RTTI pointer before the actual function pointers.

use std::ffi::c_void;

/// Windows BOOL type.
pub type BOOL = i32;
/// Windows BYTE type.
pub type BYTE = u8;
/// Windows DWORD type.
pub type DWORD = u32;
/// Windows LPCTSTR type (pointer to wide string).
pub type LPCTSTR = *const u16;

/// MSVC RTTI Complete Object Locator structure (x64 version).
/// This is placed before the vtable and pointed to by vtable[-1].
#[repr(C)]
pub struct RTTICompleteObjectLocator {
    /// Signature: 0 for x86, 1 for x64
    pub signature: u32,
    /// Offset of vfptr within the class
    pub offset: u32,
    /// Constructor displacement offset
    pub cd_offset: u32,
    /// Offset (RVA) to type descriptor
    pub p_type_descriptor: i32,
    /// Offset (RVA) to class hierarchy descriptor
    pub p_class_hierarchy_descriptor: i32,
    /// Offset (RVA) to this locator - for self-reference validation (x64 only)
    pub p_self: i32,
}

/// MSVC RTTI Type Descriptor (type_info structure).
/// Contains the mangled type name.
#[repr(C)]
pub struct RTTITypeDescriptor {
    /// Pointer to type_info vftable (we set to null as we don't need RTTI methods)
    pub p_vftable: *const c_void,
    /// Spare field (usually null)
    pub spare: *mut c_void,
    /// Mangled type name (null-terminated) - variable length but we use fixed arrays
    pub name: [u8; 32],
}

// Safety: The vftable pointer is either null or points to static memory
unsafe impl Sync for RTTITypeDescriptor {}

/// MSVC RTTI Class Hierarchy Descriptor.
/// Describes the inheritance hierarchy of a class.
#[repr(C)]
pub struct RTTIClassHierarchyDescriptor {
    /// Signature: 0 for x86, 1 for x64
    pub signature: u32,
    /// Attributes: bit 0 = multiple inheritance, bit 1 = virtual inheritance
    pub attributes: u32,
    /// Number of base classes (including self)
    pub num_base_classes: u32,
    /// RVA to array of base class descriptor pointers
    pub p_base_class_array: i32,
}

/// PMD - Pointer-to-Member Displacement for RTTI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PMD {
    /// Member displacement
    pub mdisp: i32,
    /// Vbtable displacement (-1 means no vbtable)
    pub pdisp: i32,
    /// Displacement within vbtable
    pub vdisp: i32,
}

/// MSVC RTTI Base Class Descriptor.
/// Describes one class in the inheritance hierarchy.
#[repr(C)]
pub struct RTTIBaseClassDescriptor {
    /// RVA to type descriptor
    pub p_type_descriptor: i32,
    /// Number of direct bases of this base class
    pub num_contained_bases: u32,
    /// PMD describing how to find the base within the derived object
    pub where_: PMD,
    /// Attributes
    pub attributes: u32,
    /// RVA to class hierarchy descriptor (x64 only)
    pub p_class_hierarchy_descriptor: i32,
}

/// Array of base class descriptor RVAs.
/// For IBonDriver3: [IBonDriver3, IBonDriver2, IBonDriver]
#[repr(C)]
pub struct RTTIBaseClassArray3 {
    pub entries: [i32; 3],
}

/// Complete RTTI structure for IBonDriver3 with single inheritance chain.
/// All structures are laid out contiguously so we can calculate RVAs easily.
#[repr(C)]
pub struct IBonDriver3RTTI {
    // Type descriptors
    pub type_desc_ibondriver: RTTITypeDescriptor,
    pub type_desc_ibondriver2: RTTITypeDescriptor,
    pub type_desc_ibondriver3: RTTITypeDescriptor,

    // Base class descriptors
    pub base_class_desc_ibondriver: RTTIBaseClassDescriptor,
    pub base_class_desc_ibondriver2: RTTIBaseClassDescriptor,
    pub base_class_desc_ibondriver3: RTTIBaseClassDescriptor,

    // Base class array for IBonDriver3 (includes all 3 classes)
    pub base_class_array: RTTIBaseClassArray3,

    // Class hierarchy descriptors
    pub class_hierarchy_ibondriver3: RTTIClassHierarchyDescriptor,

    // Complete object locator
    pub complete_object_locator: RTTICompleteObjectLocator,
}

/// Vtable with RTTI header.
/// The object's vfptr points to `vtable` (not to `rtti_locator`).
/// This allows TVTest to access vtable[-1] to find the RTTI locator.
#[repr(C)]
pub struct IBonDriver3VtblWithRTTI {
    /// Pointer to Complete Object Locator (at offset -8 from vtable pointer)
    pub rtti_locator_ptr: *const RTTICompleteObjectLocator,
    /// The actual vtable - object vfptr points HERE
    pub vtable: IBonDriver3Vtbl,
}

// Safety: The RTTI locator pointer points to a static constant, safe to share across threads.
unsafe impl Sync for IBonDriver3VtblWithRTTI {}

/// IBonDriver vtable.
///
/// Note: IBonDriver is NOT a COM interface, so it does NOT have QueryInterface/AddRef.
/// The vtable starts directly with OpenTuner.
///
/// C++ declaration order (from IBonDriver.hpp):
/// 0. OpenTuner
/// 1. CloseTuner
/// 2. SetChannel(BYTE)
/// 3. GetSignalLevel
/// 4. WaitTsStream
/// 5. GetReadyCount
/// 6. GetTsStream(BYTE *pDst, ...) - copy to caller's buffer
/// 7. GetTsStream(BYTE **ppDst, ...) - return pointer to internal buffer
/// 8. PurgeTsStream
/// 9. Release
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IBonDriverVtbl {
    pub open_tuner: Option<unsafe extern "system" fn(*mut c_void) -> BOOL>,
    pub close_tuner: Option<unsafe extern "system" fn(*mut c_void)>,
    pub set_channel: Option<unsafe extern "system" fn(*mut c_void, BYTE) -> BOOL>,
    pub get_signal_level: Option<unsafe extern "system" fn(*mut c_void) -> f32>,
    pub wait_ts_stream: Option<unsafe extern "system" fn(*mut c_void, DWORD) -> DWORD>,
    pub get_ready_count: Option<unsafe extern "system" fn(*mut c_void) -> DWORD>,
    /// GetTsStream - return pointer to internal buffer (second overload)
    pub get_ts_stream_ptr: Option<unsafe extern "system" fn(*mut c_void, *mut *mut BYTE, *mut DWORD, *mut DWORD) -> BOOL>,
    /// GetTsStream - copy data to caller's buffer
    pub get_ts_stream: Option<unsafe extern "system" fn(*mut c_void, *mut BYTE, *mut DWORD, *mut DWORD) -> BOOL>,
    pub purge_ts_stream: Option<unsafe extern "system" fn(*mut c_void)>,
    pub release: Option<unsafe extern "system" fn(*mut c_void)>,
}

/// IBonDriver2 vtable (extends IBonDriver).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IBonDriver2Vtbl {
    /// Base IBonDriver vtable.
    pub base: IBonDriverVtbl,

    // IBonDriver2 methods
    pub get_tuner_name: Option<unsafe extern "system" fn(*mut c_void) -> LPCTSTR>,
    pub is_tuner_opening: Option<unsafe extern "system" fn(*mut c_void) -> BOOL>,
    pub enum_tuning_space: Option<unsafe extern "system" fn(*mut c_void, DWORD) -> LPCTSTR>,
    pub enum_channel_name: Option<unsafe extern "system" fn(*mut c_void, DWORD, DWORD) -> LPCTSTR>,
    pub set_channel2: Option<unsafe extern "system" fn(*mut c_void, DWORD, DWORD) -> BOOL>,
    pub get_cur_space: Option<unsafe extern "system" fn(*mut c_void) -> DWORD>,
    pub get_cur_channel: Option<unsafe extern "system" fn(*mut c_void) -> DWORD>,
}

/// IBonDriver3 vtable (extends IBonDriver2).
///
/// C++ declaration order (from IBonDriver.hpp):
/// - GetTotalDeviceNum
/// - GetActiveDeviceNum
/// - SetLnbPower
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IBonDriver3Vtbl {
    /// Base IBonDriver2 vtable.
    pub base: IBonDriver2Vtbl,

    // IBonDriver3 methods
    pub get_total_device_num: Option<unsafe extern "system" fn(*mut c_void) -> DWORD>,
    pub get_active_device_num: Option<unsafe extern "system" fn(*mut c_void) -> DWORD>,
    pub set_lnb_power: Option<unsafe extern "system" fn(*mut c_void, BOOL) -> BOOL>,
}

/// IBonDriver object structure.
#[repr(C)]
pub struct IBonDriver {
    pub vtbl: *const IBonDriverVtbl,
}

/// IBonDriver2 object structure.
#[repr(C)]
#[allow(dead_code)]
pub struct IBonDriver2 {
    pub vtbl: *const IBonDriver2Vtbl,
}

/// IBonDriver3 object structure.
#[repr(C)]
#[allow(dead_code)]
pub struct IBonDriver3 {
    pub vtbl: *const IBonDriver3Vtbl,
}

/// Converts a Rust string to a wide string (UTF-16).
pub fn to_wide_string(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Converts a Rust Option<String> to a static wide string pointer.
/// The returned pointer is valid for the lifetime of the program.
#[allow(dead_code)]
pub fn to_static_wide_string(s: Option<&str>) -> LPCTSTR {
    match s {
        Some(s) => {
            let wide: Vec<u16> = to_wide_string(s);
            let boxed = wide.into_boxed_slice();
            let ptr = boxed.as_ptr();
            std::mem::forget(boxed); // Leak the memory
            ptr
        }
        None => std::ptr::null(),
    }
}
